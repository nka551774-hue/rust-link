use std::collections::{HashMap, HashSet, VecDeque};

use openmls::prelude::{
    hash_ref::ProposalRef,
    tls_codec::{Serialize, Size},
    *,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{json, Value};

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
    /// Preserves the monotonic frame nonce counter if the call upgrades back without a new commit.
    parked: Option<((u64, u64), MediaSender)>,
    transitions: HashMap<u16, Transition>,
    cancelled: VecDeque<u16>,
    transition_order: u64,
    last_executed: u64,
    recovery_attempts: u8,
    group_revision: u64,
    last_prepared: Option<(u64, u64)>,
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
            self.reset_group()?;
            // Discord's voice server only acts on a key package that follows its
            // external sender (op 25); until then the package is held back.
            if self.external.is_none() {
                return Ok(vec![]);
            }
            Ok(vec![self.key_package()?])
        }
    }

    pub fn ready(&self) -> bool {
        self.active.is_some()
    }

    /// Starts sending with the local group when no one else is in the call.
    ///
    /// Per "Initial Group Creation", the first member holds a group of itself
    /// and the gateway sends nothing until someone joins, so no transition
    /// would ever arrive. The caller waits a grace period first, because the
    /// roster (op 11) and a welcome may still be in flight. The key is the
    /// local group's own, so media stays encrypted; a later join moves to the
    /// next epoch through the usual commit and transition.
    pub fn start_alone(&mut self) -> Result<bool> {
        let alone = self.version == Some(1)
            && self.active.is_none()
            && self.transitions.is_empty()
            && self.expected.iter().all(|&member| member == self.user)
            && self
                .group
                .as_ref()
                .is_some_and(|g| g.epoch().as_u64() == 0 && g.members().count() == 1);
        if !alone {
            return Ok(false);
        }
        let sender = self.sender()?;
        self.prepare(0, sender, false)?;
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

    /// Encrypt an owned Opus packet without allocating once its capacity includes
    /// the 16-byte maximum DAVE footer. No mutation occurs before negotiation.
    pub fn encrypt_opus_in_place(&mut self, opus: &mut Vec<u8>) -> Result<()> {
        match self.active.as_mut() {
            Some(ActiveSender::TransportOnly) => Ok(()),
            Some(ActiveSender::Encrypted { media, .. }) => media.encrypt_in_place(opus),
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
                    let already_in_flight = self.transitions.values().any(|t| {
                        matches!(
                            t.sender,
                            TransitionSender::Encrypted { context, .. }
                                | TransitionSender::RetainEncrypted(context)
                                if context == current
                        )
                    });
                    if !already_in_flight {
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
                        let has_pending_downgrade = self
                            .transitions
                            .values()
                            .any(|t| matches!(t.sender, TransitionSender::TransportOnly));
                        if has_pending_downgrade
                            && matches!(
                                self.active,
                                Some(ActiveSender::Encrypted { context, .. }) if context == current
                            )
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
                // The gateway may execute a timed-out transition we explicitly rejected.
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
                if version == 0 {
                    let _ = data
                        .get("epoch")
                        .and_then(Value::as_u64)
                        .ok_or(Error::Protocol("invalid epoch"))?;
                    self.group_revision = self
                        .group_revision
                        .checked_add(1)
                        .ok_or(Error::Protocol("group revision exhausted"))?;
                    self.version = Some(0);
                    self.group = None;
                    self.own_commit = None;
                    self.parked = None;
                    for (id, _) in self.transitions.drain() {
                        push_cancelled(&mut self.cancelled, id);
                    }
                    self.provider = OpenMlsRustCrypto::default();
                    return Ok(vec![]);
                }
                let epoch = data
                    .get("epoch")
                    .and_then(Value::as_u64)
                    .filter(|e| *e > 0)
                    .ok_or(Error::Protocol("invalid epoch"))?;
                self.version = Some(1);
                if epoch == 1 {
                    self.recovery_attempts = 0;
                    self.reset_group()?;
                    if self.external.is_none() {
                        return Ok(vec![]);
                    }
                    return Ok(vec![self.key_package()?]);
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
                if self.external.as_ref().is_some_and(|old| old != &external) {
                    return Err(Error::Protocol("external sender changed during a voice session"));
                }
                self.external = Some(external);
                if self.version == Some(1) && self.group.is_none() {
                    self.create_group()?;
                    // The key package negotiate() held back until the external sender arrived.
                    return Ok(vec![self.key_package()?]);
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
                let result = if opcode == 29 { self.commit(body) } else { self.welcome(body) };
                match result.and_then(|_| self.sender()) {
                    Ok(sender) => self.prepare(id, sender, false),
                    Err(_) => {
                        self.recovery_attempts += 1;
                        if self.recovery_attempts > 3 {
                            return Err(Error::Protocol("repeated invalid commits or welcomes"));
                        }
                        // The existing sender is retained until a valid transition executes.
                        self.reset_group()?;
                        self.cancel_transition(id);
                        let mut outbound =
                            vec![Outbound::Json(json!({"op":31,"d":{"transition_id":id}}))];
                        if self.external.is_some() {
                            outbound.push(self.key_package()?);
                        }
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
        let mut salvaged = None;
        self.transitions.retain(|_, pending| {
            if pending.order < order {
                if matches!(sender, TransitionSender::TransportOnly) && salvaged.is_none() {
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
            } else {
                true
            }
        });
        self.activate(sender)?;
        if let Some(salvaged) = salvaged {
            self.parked = Some(salvaged);
        }
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
                if matches!(
                    self.active,
                    Some(ActiveSender::Encrypted { context, .. }) if context == expected
                ) {
                    self.parked = None;
                    return Ok(());
                }
                if self.parked.as_ref().is_some_and(|(ctx, _)| *ctx == expected) {
                    let (context, media) = self.parked.take().unwrap();
                    self.active = Some(ActiveSender::Encrypted { context, media });
                    return Ok(());
                }
                return Err(Error::Protocol("retained epoch sender is unavailable"));
            }
        }
        Ok(())
    }

    fn cancel_transition(&mut self, id: u16) {
        push_cancelled(&mut self.cancelled, id);
    }

    fn take_cancelled(&mut self, id: u16) -> bool {
        if let Some(pos) = self.cancelled.iter().position(|&item| item == id) {
            self.cancelled.remove(pos);
            true
        } else {
            false
        }
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
            if !resumed && self.last_prepared.is_some_and(|previous| previous >= *context) {
                return Err(Error::Protocol("refusing to reuse an outgoing epoch key"));
            }
            if !resumed {
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

    fn reset_group(&mut self) -> Result<()> {
        self.group_revision = self
            .group_revision
            .checked_add(1)
            .ok_or(Error::Protocol("group revision exhausted"))?;
        self.group = None;
        self.own_commit = None;
        self.parked = None;
        for (id, _) in self.transitions.drain() {
            push_cancelled(&mut self.cancelled, id);
        }
        // Discard consumed key packages and obsolete MLS storage, retaining the signing identity.
        self.provider = OpenMlsRustCrypto::default();
        if self.external.is_some() {
            self.create_group()?;
        }
        Ok(())
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
        // A bare KeyPackage, as libdave and davey send it (the clients Discord ships).
        let kp = bundle.key_package();
        let mut bytes = Vec::with_capacity(1 + kp.tls_serialized_len());
        bytes.push(26);
        check(kp.tls_serialize(&mut bytes), "key package serialization failed")?;
        Ok(Outbound::Binary(bytes))
    }

    fn proposals(&mut self, data: &[u8]) -> Result<Vec<Outbound>> {
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
                let mut pending_count = group.pending_proposals().count();
                while !remaining.is_empty() {
                    if pending_count >= MAX_PENDING_PROPOSALS {
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
                        ProtocolMessage::PublicMessage(p)
                            if p.sender()
                                == &SenderType::External(SenderExtensionIndex::new(0)) =>
                        {
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
                    check(
                        group.store_pending_proposal(self.provider.storage(), *proposal),
                        "proposal storage failed",
                    )?;
                    pending_count += 1;
                }
            }
            1 => {
                while !remaining.is_empty() {
                    let (reference, rest) = check(
                        ProposalRef::tls_deserialize_bytes(remaining),
                        "invalid proposal reference",
                    )?;
                    remaining = rest;
                    check(
                        group.remove_pending_proposal(self.provider.storage(), &reference),
                        "unknown revoked proposal",
                    )?;
                }
            }
            _ => return Err(Error::Protocol("unknown proposal operation")),
        }
        if group.pending_proposals().next().is_none() {
            return Ok(vec![]);
        }
        // Validate the resulting roster, including remove+add replacements in one epoch.
        let removed: HashSet<_> = group
            .pending_proposals()
            .filter_map(|p| match p.proposal() {
                Proposal::Remove(r) => Some(r.removed()),
                _ => None,
            })
            .collect();
        if removed.contains(&group.own_leaf_index()) {
            return Ok(vec![]);
        }
        let mut users = HashSet::new();
        for member in group.members().filter(|m| !removed.contains(&m.index)) {
            if !users.insert(identity(&member.credential)?) {
                return Err(Error::Protocol("duplicate group identity"));
            }
        }
        for p in group.pending_proposals() {
            if let Proposal::Add(add) = p.proposal() {
                if !users.insert(identity(add.key_package().leaf_node().credential())?) {
                    return Err(Error::Protocol("duplicate proposed identity"));
                }
            }
        }
        let (commit, welcome, _) = check(
            group.commit_to_pending_proposals(&self.provider, &self.signer),
            "commit creation failed",
        )?;
        let welcome_body = match welcome.as_ref().map(MlsMessageOut::body) {
            Some(MlsMessageBodyOut::Welcome(welcome)) => Some(welcome),
            Some(_) => return Err(Error::Protocol("invalid welcome format")),
            None => None,
        };
        let total_len =
            1 + commit.tls_serialized_len() + welcome_body.map_or(0, Size::tls_serialized_len);
        let mut payload = Vec::with_capacity(total_len);
        payload.push(28);
        check(commit.tls_serialize(&mut payload), "commit serialization failed")?;
        self.own_commit = Some(payload[1..].to_vec());
        if let Some(welcome) = welcome_body {
            check(welcome.tls_serialize(&mut payload), "welcome serialization failed")?;
        }
        Ok(vec![Outbound::Binary(payload)])
    }

    fn commit(&mut self, bytes: &[u8]) -> Result<()> {
        let group = self.group.as_mut().ok_or(Error::Protocol("no MLS group"))?;
        let removed: HashSet<_> = group
            .pending_proposals()
            .filter_map(|p| match p.proposal() {
                Proposal::Remove(r) => Some(r.removed()),
                _ => None,
            })
            .collect();
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
                let mut included: HashSet<&ProposalRef> = HashSet::with_capacity(known.len());
                for proposal in staged.queued_proposals() {
                    if proposal.sender() != &SenderType::External(SenderExtensionIndex::new(0))
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
            if let Some((credential, signature)) = previous.get(&member.index) {
                if credential != &member.credential || signature != &member.signature_key {
                    return Err(Error::Protocol("commit changed an existing signing identity"));
                }
            }
        }
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
        let context = staged.group_context();
        if context.group_id().as_slice() != self.channel.to_be_bytes()
            || context.ciphersuite() != SUITE
            || context.extensions().iter().count() != 1
            || context.extensions().external_senders().map(Vec::as_slice)
                != Some(std::slice::from_ref(external))
        {
            return Err(Error::Protocol("unexpected group context"));
        }
        let group = check(staged.into_group(&self.provider), "welcome adoption failed")?;
        validate_roster(&group, Some(&self.expected))?;
        self.group = Some(group);
        self.own_commit = None;
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

// Disambiguate MLS Sender from this module's outgoing media state.
use openmls::prelude::Sender as SenderType;

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
        .filter(|id| *id != 0)
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
    let id = u64::from_be_bytes(bytes);
    if id == 0 {
        return Err(Error::Protocol("zero member identity"));
    }
    Ok(id)
}

fn validate_proposal(proposal: &QueuedProposal, expected: &HashSet<u64>) -> Result<()> {
    if proposal.sender() != &SenderType::External(SenderExtensionIndex::new(0))
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

/// Decodes a canonical RFC 9420 §2.1.2 variable-length byte slice without copying.
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
    if len < min_len || bytes.len() != header_len.checked_add(len)? {
        return None;
    }
    Some(&bytes[header_len..])
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
