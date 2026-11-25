use std::collections::{HashMap, HashSet, VecDeque};

use openmls::prelude::{
    hash_ref::ProposalRef,
    tls_codec::{Serialize, Size},
    Sender as SenderType, *,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{json, Value};
use zeroize::Zeroize;

use crate::{check, media::MediaSender, Error, Result};

pub const MAX_PROTOCOL_VERSION: u16 = 1;
const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
const MAX_PENDING_TRANSITIONS: usize = 16;
const MAX_CANCELLED_TRANSITIONS: usize = 32;
const MAX_EXPECTED_MEMBERS: usize = 4096;
const MAX_PENDING_PROPOSALS: usize = MAX_EXPECTED_MEMBERS * 2;

/// Messages for the authenticated Discord voice gateway, without server sequence prefixes.
pub enum Outbound {
    Json(Value),
    Binary(Vec<u8>),
}

enum ActiveSender {
    TransportOnly,
    Encrypted { context: (u64, u64), media: MediaSender },
}

enum TransitionSender {
    TransportOnly,
    Encrypted { context: (u64, u64), media: MediaSender },
    RetainEncrypted((u64, u64)),
}

impl TransitionSender {
    fn targets(&self, target: (u64, u64)) -> bool {
        matches!(
            self,
            Self::Encrypted { context, .. } | Self::RetainEncrypted(context) if *context == target
        )
    }
}

struct Transition {
    order: u64,
    sender: TransitionSender,
}

/// One voice session, driven by the gateway task that also encrypts outgoing frames.
/// MLS advances when a commit arrives; the sender advances only on Execute Transition.
pub struct Session {
    user: u64,
    channel: u64,
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
    external: Option<ExternalSender>,
    group: Option<MlsGroup>,
    version: Option<u16>,
    expected: HashSet<u64>,
    own_commit: Option<Vec<u8>>,
    active: Option<ActiveSender>,
    /// Encrypted sender temporarily displaced by a transport-only downgrade on the same epoch.
    parked: Option<((u64, u64), MediaSender)>,
    transitions: HashMap<u16, Transition>,
    cancelled: VecDeque<u16>,
    transition_order: u64,
    last_executed: u64,
    recovery_attempts: u8,
    group_revision: u64,
    last_prepared: Option<(u64, u64)>,
    alone_started: bool,
}

impl Session {
    pub fn new(user: u64, channel: u64) -> Result<Self> {
        if user == 0 || channel == 0 {
            return Err(Error::Protocol("invalid user or channel ID"));
        }
        let signer = check(
            SignatureKeyPair::new(SUITE.signature_algorithm()),
            "signer initialization failed",
        )?;
        let credential = CredentialWithKey {
            credential: BasicCredential::new(user.to_be_bytes().to_vec()).into(),
            signature_key: signer.public().into(),
        };
        Ok(Self {
            user,
            channel,
            provider: OpenMlsRustCrypto::default(),
            signer,
            credential,
            external: None,
            group: None,
            version: None,
            expected: HashSet::from([user]),
            own_commit: None,
            active: None,
            parked: None,
            transitions: HashMap::new(),
            cancelled: VecDeque::with_capacity(MAX_CANCELLED_TRANSITIONS),
            transition_order: 0,
            last_executed: 0,
            recovery_attempts: 0,
            group_revision: 0,
            last_prepared: None,
            alone_started: false,
        })
    }

    /// Apply the version selected by Session Description. No implicit cleartext fallback.
    pub fn negotiate(&mut self, version: u16) -> Result<Vec<Outbound>> {
        validate_version(version)?;
        if self.version.is_some() {
            return Err(Error::Protocol("duplicate session description"));
        }
        self.version = Some(version);
        if version == 0 {
            self.active = Some(ActiveSender::TransportOnly);
            Ok(vec![])
        } else {
            self.reset_and_key_package()
        }
    }

    pub fn ready(&self) -> bool {
        self.active.is_some()
    }

    /// Starts sending with the local group when no one else is in the call after a grace period.
    pub fn start_alone(&mut self) -> Result<bool> {
        let alone = self.version == Some(1)
            && matches!(self.active, None | Some(ActiveSender::TransportOnly))
            && self.transitions.is_empty()
            && self.expected.iter().all(|&m| m == self.user)
            && self
                .group
                .as_ref()
                .is_some_and(|g| g.epoch().as_u64() == 0 && g.members().count() == 1);
        if !alone {
            return Ok(false);
        }
        let sender = self.sender()?;
        self.prepare(0, sender, false)?;
        self.alone_started = true;
        Ok(true)
    }

    pub fn active_version(&self) -> Option<u16> {
        self.active.as_ref().map(|s| match s {
            ActiveSender::TransportOnly => 0,
            ActiveSender::Encrypted { .. } => 1,
        })
    }

    pub fn encrypt_opus(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        match self.active.as_mut() {
            Some(ActiveSender::TransportOnly) => Ok(opus.to_vec()),
            Some(ActiveSender::Encrypted { media, .. }) => media.encrypt(opus),
            None => Err(Error::NotReady),
        }
    }

    #[inline]
    pub fn encrypt_opus_in_place(&mut self, opus: &mut Vec<u8>) -> Result<()> {
        self.encrypt_opus_in_place_from(opus, 0)
    }

    /// Encrypt `frame[payload_offset..]` in place without touching `frame[..payload_offset]`.
    #[inline]
    pub fn encrypt_opus_in_place_from(
        &mut self,
        frame: &mut Vec<u8>,
        payload_offset: usize,
    ) -> Result<()> {
        match self.active.as_mut() {
            Some(ActiveSender::TransportOnly) if payload_offset <= frame.len() => Ok(()),
            Some(ActiveSender::TransportOnly) => {
                Err(Error::Protocol("payload offset exceeds frame length"))
            }
            Some(ActiveSender::Encrypted { media, .. }) => {
                media.encrypt_in_place_from(frame, payload_offset)
            }
            None => Err(Error::NotReady),
        }
    }

    pub fn handle_json(&mut self, opcode: u8, data: &Value) -> Result<Vec<Outbound>> {
        if matches!(opcode, 21 | 22 | 24) && self.version.is_none() {
            return Err(Error::Protocol("DAVE transition before session description"));
        }
        match opcode {
            11 => {
                let ids = data
                    .get("user_ids")
                    .and_then(Value::as_array)
                    .ok_or(Error::Protocol("invalid client list"))?;
                if ids.len() > MAX_EXPECTED_MEMBERS {
                    return Err(Error::Protocol("excessive client list"));
                }
                for id in ids {
                    let parsed = snowflake(id)?;
                    if self.expected.len() >= MAX_EXPECTED_MEMBERS
                        && !self.expected.contains(&parsed)
                    {
                        return Err(Error::Protocol("excessive client list"));
                    }
                    self.expected.insert(parsed);
                }
            }
            13 => {
                let id = snowflake(&data["user_id"])?;
                if id != self.user {
                    self.expected.remove(&id);
                }
            }
            21 => {
                let version = number(data, "protocol_version")?;
                validate_version(version)?;
                let id = number(data, "transition_id")?;
                if version == 0 {
                    return self.prepare(id, TransitionSender::TransportOnly, false);
                }
                if self.group.is_some() {
                    self.version = Some(1);
                }
                if let Some(current) = self.current_context() {
                    let in_flight = self
                        .transitions
                        .values()
                        .max_by_key(|t| t.order)
                        .is_some_and(|t| t.sender.targets(current));
                    if !in_flight {
                        let has_sender = matches!(
                            self.active,
                            Some(ActiveSender::Encrypted { context, .. }) if context == current
                        ) || self.parked.as_ref().is_some_and(|(ctx, _)| *ctx == current)
                            || self.transitions.values().any(|t| {
                                matches!(t.sender, TransitionSender::Encrypted { context, .. } if context == current)
                            });
                        if std::mem::take(&mut self.alone_started) && has_sender {
                            return self.prepare(
                                id,
                                TransitionSender::RetainEncrypted(current),
                                true,
                            );
                        }
                        if matches!(self.active, Some(ActiveSender::TransportOnly))
                            && self.parked.as_ref().is_some_and(|(ctx, _)| *ctx == current)
                        {
                            let (context, media) = self.parked.take().unwrap();
                            return self.prepare(
                                id,
                                TransitionSender::Encrypted { context, media },
                                true,
                            );
                        }
                        if has_sender
                            && self
                                .transitions
                                .values()
                                .any(|t| matches!(t.sender, TransitionSender::TransportOnly))
                        {
                            return self.prepare(
                                id,
                                TransitionSender::RetainEncrypted(current),
                                true,
                            );
                        }
                    }
                }
                let sender = self.sender()?;
                return self.prepare(id, sender, false);
            }
            22 => {
                let id = number(data, "transition_id")?;
                if self.take_cancelled(id) {
                    return Ok(vec![]);
                }
                let transition =
                    self.transitions.remove(&id).ok_or(Error::Protocol("unknown transition"))?;
                if transition.order <= self.last_executed {
                    return Err(Error::Protocol("out-of-order transition"));
                }
                self.execute_transition(transition.order, transition.sender)?;
            }
            24 => {
                let version = number(data, "protocol_version")?;
                validate_version(version)?;
                let epoch = data
                    .get("epoch")
                    .and_then(Value::as_u64)
                    .ok_or(Error::Protocol("invalid epoch"))?;
                if version == 0 {
                    self.clear_group()?;
                    self.version = Some(0);
                    return Ok(vec![]);
                }
                if epoch == 0 {
                    return Err(Error::Protocol("invalid epoch"));
                }
                self.version = Some(1);
                if epoch == 1 {
                    self.recovery_attempts = 0;
                    return self.reset_and_key_package();
                }
                if self.group.as_ref().map(|g| g.epoch().as_u64()) != Some(epoch) {
                    return Err(Error::Protocol("unexpected epoch"));
                }
            }
            _ => {}
        }
        Ok(vec![])
    }

    pub fn handle_binary(&mut self, opcode: u8, data: &[u8]) -> Result<Vec<Outbound>> {
        match opcode {
            25 => {
                let external = check(
                    ExternalSender::tls_deserialize_exact_bytes(data),
                    "invalid external sender",
                )?;
                let changed = self.external.as_ref().is_some_and(|old| old != &external);
                if changed && !self.group.as_ref().is_none_or(|g| g.epoch().as_u64() == 0) {
                    return Err(Error::Protocol("external sender changed during a voice session"));
                }
                self.external = Some(external);
                if self.version == Some(1) {
                    if self.group.is_none() {
                        self.create_group()?;
                        return Ok(vec![self.key_package()?]);
                    }
                    if changed {
                        return self.reset_and_key_package();
                    }
                }
                Ok(vec![])
            }
            27 => self.proposals(data),
            29 | 30 => {
                if self.version != Some(1) {
                    return Err(Error::Protocol("MLS message before DAVE negotiation"));
                }
                let Some((id_bytes, body)) =
                    data.split_first_chunk::<2>().filter(|(_, body)| !body.is_empty())
                else {
                    return Err(Error::Protocol("truncated commit or welcome"));
                };
                let id = u16::from_be_bytes(*id_bytes);
                let res = if opcode == 29 { self.commit(body) } else { self.welcome(body) };
                match res.and_then(|()| self.sender()) {
                    Ok(sender) => self.prepare(id, sender, false),
                    Err(_) => {
                        self.recovery_attempts += 1;
                        if self.recovery_attempts > 3 {
                            return Err(Error::Protocol("repeated invalid commits or welcomes"));
                        }
                        let mut outbound =
                            vec![Outbound::Json(json!({"op":31,"d":{"transition_id":id}}))];
                        outbound.extend(self.reset_and_key_package()?);
                        push_cancelled(&mut self.cancelled, id);
                        Ok(outbound)
                    }
                }
            }
            _ => Err(Error::Protocol("unexpected binary opcode")),
        }
    }

    fn current_context(&self) -> Option<(u64, u64)> {
        self.group.as_ref().map(|g| (self.group_revision, g.epoch().as_u64()))
    }

    fn execute_transition(&mut self, order: u64, sender: TransitionSender) -> Result<()> {
        let current = self.current_context();
        let can_salvage = matches!(
            sender,
            TransitionSender::TransportOnly | TransitionSender::RetainEncrypted(_)
        );
        let mut salvaged = None;
        self.transitions.retain(|_, pending| {
            if pending.order >= order {
                return true;
            }
            if can_salvage && salvaged.is_none() {
                let dummy = Transition { order: 0, sender: TransitionSender::TransportOnly };
                if let TransitionSender::Encrypted { context, media } =
                    std::mem::replace(pending, dummy).sender
                {
                    if Some(context) == current {
                        salvaged = Some((context, media));
                    }
                }
            }
            false
        });
        if let Some(salvaged) = salvaged {
            self.parked = Some(salvaged);
        }
        self.activate(sender)?;
        self.last_executed = order;
        self.recovery_attempts = 0;
        Ok(())
    }

    fn activate(&mut self, sender: TransitionSender) -> Result<()> {
        match sender {
            TransitionSender::TransportOnly => {
                if let Some(ActiveSender::Encrypted { context, media }) = self.active.take() {
                    if Some(context) == self.current_context() {
                        self.parked = Some((context, media));
                    }
                }
                self.active = Some(ActiveSender::TransportOnly);
            }
            TransitionSender::Encrypted { context, media } => {
                self.parked = None;
                self.active = Some(ActiveSender::Encrypted { context, media });
            }
            TransitionSender::RetainEncrypted(expected) => {
                if matches!(self.active, Some(ActiveSender::Encrypted { context, .. }) if context == expected)
                {
                    self.parked = None;
                } else if self.parked.as_ref().is_some_and(|(ctx, _)| *ctx == expected) {
                    let (context, media) = self.parked.take().unwrap();
                    self.active = Some(ActiveSender::Encrypted { context, media });
                } else {
                    return Err(Error::Protocol("retained epoch sender is unavailable"));
                }
            }
        }
        Ok(())
    }

    fn take_cancelled(&mut self, id: u16) -> bool {
        let Some(pos) = self.cancelled.iter().position(|&item| item == id) else {
            return false;
        };
        self.cancelled.remove(pos);
        true
    }

    fn prepare(
        &mut self,
        id: u16,
        sender: TransitionSender,
        resumed: bool,
    ) -> Result<Vec<Outbound>> {
        if id != 0
            && (self.transitions.len() >= MAX_PENDING_TRANSITIONS
                || self.transitions.contains_key(&id))
        {
            return Err(Error::Protocol("duplicate or excessive pending transitions"));
        }
        if let TransitionSender::Encrypted { context, .. } = &sender {
            if !resumed {
                if self.last_prepared.is_some_and(|prev| prev >= *context) {
                    return Err(Error::Protocol("refusing to reuse an outgoing epoch key"));
                }
                self.last_prepared = Some(*context);
            }
        }
        self.transition_order = self
            .transition_order
            .checked_add(1)
            .ok_or(Error::Protocol("transition counter exhausted"))?;
        self.take_cancelled(id);
        if id == 0 {
            self.execute_transition(self.transition_order, sender)?;
            return Ok(vec![]);
        }
        self.transitions.insert(id, Transition { order: self.transition_order, sender });
        Ok(vec![Outbound::Json(json!({"op":23,"d":{"transition_id":id}}))])
    }

    fn cancel_mls_transitions(&mut self) {
        self.transitions.retain(|&id, t| {
            let keep = matches!(t.sender, TransitionSender::TransportOnly);
            if !keep {
                push_cancelled(&mut self.cancelled, id);
            }
            keep
        });
    }

    fn clear_group(&mut self) -> Result<()> {
        self.group_revision = self
            .group_revision
            .checked_add(1)
            .ok_or(Error::Protocol("group revision exhausted"))?;
        self.group = None;
        self.own_commit = None;
        self.parked = None;
        self.alone_started = false;
        self.cancel_mls_transitions();
        zeroize_provider(&self.provider);
        self.provider = OpenMlsRustCrypto::default();
        Ok(())
    }

    fn reset_group(&mut self) -> Result<()> {
        self.clear_group()?;
        if self.external.is_some() {
            self.create_group()?;
        }
        Ok(())
    }

    fn reset_and_key_package(&mut self) -> Result<Vec<Outbound>> {
        self.reset_group()?;
        if self.external.is_some() {
            Ok(vec![self.key_package()?])
        } else {
            Ok(vec![])
        }
    }

    fn create_group(&mut self) -> Result<()> {
        let external = self.external.clone().ok_or(Error::Protocol("no external sender"))?;
        let extensions = check(
            Extensions::single(Extension::ExternalSenders(vec![external])),
            "invalid extensions",
        )?;
        let config = MlsGroupCreateConfig::builder()
            .with_group_context_extensions(extensions)
            .ciphersuite(SUITE)
            .capabilities(capabilities())
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build();
        self.group = Some(check(
            MlsGroup::new_with_group_id(
                &self.provider,
                &self.signer,
                &config,
                GroupId::from_slice(&self.channel.to_be_bytes()),
                self.credential.clone(),
            ),
            "group creation failed",
        )?);
        Ok(())
    }

    fn key_package(&self) -> Result<Outbound> {
        let mut lifetime = [0xff; 16];
        lifetime[..8].fill(0);
        let lifetime = check(Lifetime::tls_deserialize_exact_bytes(&lifetime), "invalid lifetime")?;
        let bundle = check(
            KeyPackage::builder()
                .key_package_extensions(Extensions::empty())
                .leaf_node_capabilities(capabilities())
                .key_package_lifetime(lifetime)
                .build(SUITE, &self.provider, &self.signer, self.credential.clone()),
            "key package creation failed",
        )?;
        let kp = bundle.key_package();
        let mut bytes = Vec::with_capacity(1 + kp.tls_serialized_len());
        bytes.push(26);
        check(kp.tls_serialize(&mut bytes), "key package serialization failed")?;
        Ok(Outbound::Binary(bytes))
    }

    fn proposals(&mut self, data: &[u8]) -> Result<Vec<Outbound>> {
        if self.version.is_none() {
            return Err(Error::Protocol("MLS message before DAVE negotiation"));
        }
        let Some(group) = self.group.as_mut() else {
            return Ok(vec![]);
        };
        let (&operation, bytes) =
            data.split_first().ok_or(Error::Protocol("missing proposal operation"))?;
        let mut remaining =
            read_vlbytes_exact(bytes).ok_or(Error::Protocol("invalid proposal vector"))?;
        check(group.clear_pending_commit(self.provider.storage()), "cannot clear pending commit")?;
        self.own_commit = None;
        match operation {
            0 => {
                let pending_count = group.pending_proposals().count();
                let mut staged = Vec::new();
                while !remaining.is_empty() {
                    if pending_count + staged.len() >= MAX_PENDING_PROPOSALS {
                        return Err(Error::Protocol("excessive pending proposals"));
                    }
                    let (message, rest) = check(
                        MlsMessageIn::tls_deserialize_bytes(remaining),
                        "invalid proposal message",
                    )?;
                    remaining = rest;
                    let protocol =
                        check(message.try_into_protocol_message(), "invalid proposal format")?;
                    match &protocol {
                        ProtocolMessage::PublicMessage(p) if is_external_sender(p.sender()) => {
                            if self.recovery_attempts > 0
                                && group.epoch().as_u64() == 0
                                && p.epoch().as_u64() != 0
                            {
                                continue;
                            }
                        }
                        _ => {
                            return Err(Error::Protocol(
                                "proposal is not from the trusted external sender",
                            ))
                        }
                    }
                    let processed = check(
                        group.process_message(&self.provider, protocol),
                        "proposal verification failed",
                    )?;
                    let ProcessedMessageContent::ProposalMessage(proposal) =
                        processed.into_content()
                    else {
                        return Err(Error::Protocol("expected a proposal"));
                    };
                    validate_proposal(&proposal, &self.expected)?;
                    staged.push(*proposal);
                }
                check_projected_roster(group, group.pending_proposals().chain(&staged))?;
                for proposal in staged {
                    check(
                        group.store_pending_proposal(self.provider.storage(), proposal),
                        "proposal storage failed",
                    )?;
                }
            }
            1 => {
                let mut references = Vec::new();
                {
                    let mut known: HashSet<&ProposalRef> =
                        group.pending_proposals().map(|p| p.proposal_reference_ref()).collect();
                    while !remaining.is_empty() {
                        let (reference, rest) = check(
                            ProposalRef::tls_deserialize_bytes(remaining),
                            "invalid proposal reference",
                        )?;
                        remaining = rest;
                        if !known.remove(&reference) {
                            return Err(Error::Protocol("unknown revoked proposal"));
                        }
                        references.push(reference);
                    }
                }
                for reference in &references {
                    check(
                        group.remove_pending_proposal(self.provider.storage(), reference),
                        "unknown revoked proposal",
                    )?;
                }
            }
            _ => return Err(Error::Protocol("unknown proposal operation")),
        }
        if group.pending_proposals().next().is_none() {
            return Ok(vec![]);
        }
        let removed = check_projected_roster(group, group.pending_proposals())?;
        if removed.contains(&group.own_leaf_index()) {
            return Ok(vec![]);
        }
        let (commit, welcome, _) = check(
            group.commit_to_pending_proposals(&self.provider, &self.signer),
            "commit creation failed",
        )?;
        let welcome_body = match welcome.as_ref().map(MlsMessageOut::body) {
            Some(MlsMessageBodyOut::Welcome(w)) => Some(w),
            Some(_) => return Err(Error::Protocol("invalid welcome format")),
            None => None,
        };
        let total_len =
            1 + commit.tls_serialized_len() + welcome_body.map_or(0, Size::tls_serialized_len);
        let mut payload = Vec::with_capacity(total_len);
        payload.push(28);
        check(commit.tls_serialize(&mut payload), "commit serialization failed")?;
        self.own_commit = Some(payload[1..].to_vec());
        if let Some(w) = welcome_body {
            check(w.tls_serialize(&mut payload), "welcome serialization failed")?;
        }
        Ok(vec![Outbound::Binary(payload)])
    }

    fn commit(&mut self, bytes: &[u8]) -> Result<()> {
        let group = self.group.as_mut().ok_or(Error::Protocol("no MLS group"))?;
        let removed = removed_leaves(group.pending_proposals());
        let previous: HashMap<_, _> = group
            .members()
            .filter(|m| !removed.contains(&m.index))
            .map(|m| (m.index, (m.credential, m.signature_key)))
            .collect();
        let own = self.own_commit.as_deref() == Some(bytes);
        if group.epoch().as_u64() == 0 && !own {
            return Err(Error::Protocol("foreign initial commit"));
        }
        if own {
            check(group.merge_pending_commit(&self.provider), "own commit merge failed")?;
        } else {
            let message =
                check(MlsMessageIn::tls_deserialize_exact_bytes(bytes), "invalid commit")?;
            let protocol = check(message.try_into_protocol_message(), "invalid commit format")?;
            if !matches!(&protocol, ProtocolMessage::PublicMessage(_)) {
                return Err(Error::Protocol("private commit"));
            }
            let processed = check(
                group.process_message(&self.provider, protocol),
                "commit verification failed",
            )?;
            let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content()
            else {
                return Err(Error::Protocol("expected commit"));
            };
            {
                let known: HashSet<&ProposalRef> =
                    group.pending_proposals().map(|p| p.proposal_reference_ref()).collect();
                let mut included = HashSet::with_capacity(known.len());
                for proposal in staged.queued_proposals() {
                    if !is_external_sender(proposal.sender())
                        || proposal.proposal_or_ref_type() != ProposalOrRefType::Reference
                        || !matches!(proposal.proposal(), Proposal::Add(_) | Proposal::Remove(_))
                    {
                        return Err(Error::Protocol("unauthorized or inline commit proposal"));
                    }
                    let reference = proposal.proposal_reference_ref();
                    if !known.contains(reference) || !included.insert(reference) {
                        return Err(Error::Protocol("uncached or duplicate commit proposal"));
                    }
                }
                if known.is_empty() || known.len() != included.len() {
                    return Err(Error::Protocol("commit omitted proposals"));
                }
            }
            check(group.merge_staged_commit(&self.provider, *staged), "commit merge failed")?;
        }
        self.own_commit = None;
        validate_roster(group, None)?;
        for member in group.members() {
            if let Some((cred, sig)) = previous.get(&member.index) {
                if cred != &member.credential || sig != &member.signature_key {
                    return Err(Error::Protocol("commit changed an existing signing identity"));
                }
            }
        }
        self.alone_started = false;
        Ok(())
    }

    fn welcome(&mut self, bytes: &[u8]) -> Result<()> {
        if self.group.as_ref().is_some_and(|g| g.epoch().as_u64() > 0) {
            return Err(Error::Protocol("already in an established group"));
        }
        let external = self.external.as_ref().ok_or(Error::Protocol("no external sender"))?;
        let welcome = check(Welcome::tls_deserialize_exact_bytes(bytes), "invalid welcome")?;
        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build();
        let staged = check(
            check(
                StagedWelcome::build_from_welcome(&self.provider, &config, welcome),
                "welcome staging failed",
            )?
            .replace_old_group()
            .build(),
            "welcome verification failed",
        )?;
        let ctx = staged.group_context();
        if ctx.epoch().as_u64() < 1
            || ctx.group_id().as_slice() != self.channel.to_be_bytes()
            || ctx.ciphersuite() != SUITE
            || ctx.extensions().iter().count() != 1
            || ctx.extensions().external_senders().map(Vec::as_slice)
                != Some(std::slice::from_ref(external))
        {
            return Err(Error::Protocol("unexpected group context"));
        }
        let group = check(staged.into_group(&self.provider), "welcome adoption failed")?;
        validate_roster(&group, Some(&self.expected))?;
        self.group = Some(group);
        self.own_commit = None;
        self.alone_started = false;
        Ok(())
    }

    fn sender(&self) -> Result<TransitionSender> {
        let group = self.group.as_ref().ok_or(Error::NotReady)?;
        let secret = check(
            group.export_secret(
                self.provider.crypto(),
                "Discord Secure Frames v0",
                &self.user.to_le_bytes(),
                16,
            ),
            "sender secret export failed",
        )?;
        Ok(TransitionSender::Encrypted {
            context: (self.group_revision, group.epoch().as_u64()),
            media: MediaSender::new(secret),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        zeroize_provider(&self.provider);
    }
}

fn zeroize_provider(provider: &OpenMlsRustCrypto) {
    let mut values = provider.storage().values.write().unwrap_or_else(|p| p.into_inner());
    for (mut key, mut value) in values.drain() {
        key.zeroize();
        value.zeroize();
    }
}

#[inline]
fn is_external_sender(sender: &SenderType) -> bool {
    *sender == SenderType::External(SenderExtensionIndex::new(0))
}

fn removed_leaves<'a>(
    proposals: impl IntoIterator<Item = &'a QueuedProposal>,
) -> HashSet<LeafNodeIndex> {
    proposals
        .into_iter()
        .filter_map(|p| match p.proposal() {
            Proposal::Remove(r) => Some(r.removed()),
            _ => None,
        })
        .collect()
}

fn check_projected_roster<'a>(
    group: &MlsGroup,
    proposals: impl IntoIterator<Item = &'a QueuedProposal>,
) -> Result<HashSet<LeafNodeIndex>> {
    let mut removed = HashSet::new();
    let mut added = Vec::new();
    for p in proposals {
        match p.proposal() {
            Proposal::Remove(r) => {
                removed.insert(r.removed());
            }
            Proposal::Add(add) => {
                added.push(identity(add.key_package().leaf_node().credential())?);
            }
            _ => {}
        }
    }
    let mut users = HashSet::new();
    for member in group.members().filter(|m| !removed.contains(&m.index)) {
        if !users.insert(identity(&member.credential)?) {
            return Err(Error::Protocol("duplicate group identity"));
        }
    }
    for id in added {
        if !users.insert(id) {
            return Err(Error::Protocol("duplicate proposed identity"));
        }
    }
    Ok(removed)
}

fn validate_version(version: u16) -> Result<()> {
    if version > MAX_PROTOCOL_VERSION {
        Err(Error::Protocol("unsupported protocol version"))
    } else {
        Ok(())
    }
}

fn number(data: &Value, field: &str) -> Result<u16> {
    data.get(field)
        .and_then(Value::as_u64)
        .and_then(|v| v.try_into().ok())
        .ok_or(Error::Protocol("invalid transition field"))
}

fn snowflake(value: &Value) -> Result<u64> {
    value
        .as_str()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .ok_or(Error::Protocol("invalid user ID"))
}

fn identity(credential: &Credential) -> Result<u64> {
    if credential.credential_type() != CredentialType::Basic {
        return Err(Error::Protocol("non-basic member credential"));
    }
    let bytes: [u8; 8] = credential
        .serialized_content()
        .try_into()
        .map_err(|_| Error::Protocol("invalid member credential"))?;
    match u64::from_be_bytes(bytes) {
        0 => Err(Error::Protocol("zero member identity")),
        id => Ok(id),
    }
}

fn validate_proposal(proposal: &QueuedProposal, expected: &HashSet<u64>) -> Result<()> {
    if !is_external_sender(proposal.sender())
        || proposal.proposal_or_ref_type() != ProposalOrRefType::Reference
    {
        return Err(Error::Protocol("unauthorized proposal sender or inline proposal"));
    }
    match proposal.proposal() {
        Proposal::Add(add)
            if expected.contains(&identity(add.key_package().leaf_node().credential())?) =>
        {
            Ok(())
        }
        Proposal::Remove(_) => Ok(()),
        _ => Err(Error::Protocol("unexpected member or proposal type")),
    }
}

fn validate_roster(group: &MlsGroup, expected: Option<&HashSet<u64>>) -> Result<()> {
    if !group.is_active() {
        return Err(Error::Protocol("local member was removed"));
    }
    let mut seen = HashSet::new();
    for member in group.members() {
        let id = identity(&member.credential)?;
        if !seen.insert(id) || expected.is_some_and(|ids| !ids.contains(&id)) {
            return Err(Error::Protocol("duplicate or unexpected group member"));
        }
    }
    Ok(())
}

fn capabilities() -> Capabilities {
    Capabilities::builder()
        .versions(vec![ProtocolVersion::Mls10])
        .ciphersuites(vec![SUITE])
        .extensions(vec![])
        .proposals(vec![])
        .credentials(vec![CredentialType::Basic])
        .build()
}

fn push_cancelled(cancelled: &mut VecDeque<u16>, id: u16) {
    if !cancelled.contains(&id) {
        if cancelled.len() >= MAX_CANCELLED_TRANSITIONS {
            cancelled.pop_front();
        }
        cancelled.push_back(id);
    }
}

/// Decodes a canonical RFC 9420 section 2.1.2 variable-length byte slice without copying.
fn read_vlbytes_exact(bytes: &[u8]) -> Option<&[u8]> {
    let &first = bytes.first()?;
    let (header_len, len, min_len): (usize, usize, usize) = match first >> 6 {
        0 => (1, usize::from(first & 0x3f), 0),
        1 => (2, usize::from(u16::from_be_bytes([first & 0x3f, *bytes.get(1)?])), 1 << 6),
        2 => (
            4,
            u32::from_be_bytes([first & 0x3f, *bytes.get(1)?, *bytes.get(2)?, *bytes.get(3)?])
                as usize,
            1 << 14,
        ),
        _ => return None,
    };
    (len >= min_len && bytes.len() == header_len.checked_add(len)?).then(|| &bytes[header_len..])
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
