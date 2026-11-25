use super::*;
use davey::{DaveSession, MediaType, ProposalsOperationType};
use std::num::NonZeroU16;

const CHANNEL: u64 = 9_000_000;
const ALICE: u64 = 1_000_001;
const BOB: u64 = 1_000_002;
const CAROL: u64 = 1_000_003;
const OPUS: &[u8] = &[0xf8, 0x12, 0x23, 0x34, 0x45, 0x56];

struct Host {
    signer: SignatureKeyPair,
    external: Vec<u8>,
}

impl Host {
    fn new() -> Self {
        let signer = SignatureKeyPair::new(SUITE.signature_algorithm()).unwrap();
        let external = ExternalSender::new(
            signer.public().into(),
            BasicCredential::new(b"test voice gateway".to_vec()).into(),
        )
        .tls_serialize_detached()
        .unwrap();
        Self { signer, external }
    }

    fn peer(&self, user: u64) -> DaveSession {
        let mut peer = DaveSession::new(NonZeroU16::new(1).unwrap(), user, CHANNEL, None).unwrap();
        peer.set_external_sender(&self.external).unwrap();
        peer
    }

    fn session(&self, user: u64) -> (Session, Vec<u8>) {
        let mut session = Session::new(user, CHANNEL).unwrap();
        session.handle_binary(25, &self.external).unwrap();
        let key_package = take_binary(session.negotiate(1).unwrap(), 26);
        session
            .handle_json(
                11,
                &json!({"user_ids":[ALICE.to_string(),BOB.to_string(),CAROL.to_string()]}),
            )
            .unwrap();
        (session, key_package)
    }

    fn add(&self, epoch: u64, raw_key_package: &[u8]) -> Vec<u8> {
        let provider = OpenMlsRustCrypto::default();
        let kp = KeyPackageIn::tls_deserialize_exact_bytes(raw_key_package)
            .unwrap()
            .validate(provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        let msg = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            kp,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        VLBytes::new(msg.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()
    }

    fn add_peer(&self, epoch: u64, peer: &mut DaveSession) -> Vec<u8> {
        self.add(epoch, &peer.create_key_package().unwrap())
    }

    fn remove(&self, epoch: u64, index: LeafNodeIndex) -> Vec<u8> {
        let msg = ExternalProposal::new_remove::<OpenMlsRustCrypto>(
            index,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        VLBytes::new(msg.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()
    }
}

fn take_binary(messages: Vec<Outbound>, opcode: u8) -> Vec<u8> {
    assert_eq!(messages.len(), 1);
    match messages.into_iter().next().unwrap() {
        Outbound::Binary(bytes) => {
            assert_eq!(bytes[0], opcode);
            bytes[1..].to_vec()
        }
        _ => panic!("expected a binary response"),
    }
}

fn op27(op: u8, vl_encoded: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + vl_encoded.len());
    payload.push(op);
    payload.extend_from_slice(vl_encoded);
    payload
}

fn op27_vl(op: u8, raw: Vec<u8>) -> Vec<u8> {
    op27(op, &VLBytes::new(raw).tls_serialize_detached().unwrap())
}

fn append(session: &mut Session, proposals: &[u8]) -> Vec<u8> {
    take_binary(session.handle_binary(27, &op27(0, proposals)).unwrap(), 28)
}

fn split_commit_welcome(payload: &[u8]) -> (&[u8], &[u8]) {
    let (_, rest) = MlsMessageIn::tls_deserialize_bytes(payload).unwrap();
    (&payload[..payload.len() - rest.len()], rest)
}

fn transition(session: &mut Session, op: u8, id: u16, bytes: &[u8]) -> Vec<Outbound> {
    let mut payload = id.to_be_bytes().to_vec();
    payload.extend_from_slice(bytes);
    session.handle_binary(op, &payload).unwrap()
}

fn welcome_peer(
    session: &mut Session,
    proposal: &[u8],
    id: u16,
    peer: &mut DaveSession,
) -> Vec<Outbound> {
    let payload = append(session, proposal);
    let (commit, welcome) = split_commit_welcome(&payload);
    let ready = transition(session, 29, id, commit);
    peer.process_welcome(welcome).unwrap();
    ready
}

fn prepare_v(session: &mut Session, version: u16, id: u16) -> Result<Vec<Outbound>> {
    session.handle_json(21, &json!({"protocol_version":version,"transition_id":id}))
}

fn execute_res(session: &mut Session, id: u16) -> Result<Vec<Outbound>> {
    session.handle_json(22, &json!({"transition_id":id}))
}

fn execute(session: &mut Session, id: u16) {
    execute_res(session, id).unwrap();
}

fn announce_epoch(session: &mut Session, version: u16, epoch: u64) -> Result<Vec<Outbound>> {
    session.handle_json(24, &json!({"protocol_version":version,"epoch":epoch}))
}

fn send_and_verify(session: &mut Session, peer: &mut DaveSession) -> Vec<u8> {
    let encrypted = session.encrypt_opus(OPUS).unwrap();
    assert_eq!(peer.decrypt(ALICE, MediaType::AUDIO, &encrypted).unwrap(), OPUS);
    encrypted
}

fn established() -> (Host, Session, DaveSession) {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    let proposal = host.add_peer(0, &mut peer);
    welcome_peer(&mut session, &proposal, 1, &mut peer);
    execute(&mut session, 1);
    (host, session, peer)
}

#[test]
fn own_initial_commit_is_interoperable_and_waits_for_execute() {
    let host = Host::new();
    let (mut session, key_package) = host.session(ALICE);
    assert!(KeyPackageIn::tls_deserialize_exact_bytes(&key_package).is_ok());
    // Bare KeyPackage carries cipher_suite=0x0002 at bytes 2..4, never MlsMessage wire_format=0x0005.
    assert_eq!(&key_package[..4], &[0x00, 0x01, 0x00, 0x02]);
    let mut peer = host.peer(BOB);
    let proposal = host.add_peer(0, &mut peer);
    let ready = welcome_peer(&mut session, &proposal, 7, &mut peer);
    assert!(
        matches!(&ready[0], Outbound::Json(v) if v["op"] == 23 && v["d"]["transition_id"] == 7)
    );
    assert!(matches!(session.encrypt_opus(OPUS), Err(Error::NotReady)));
    execute(&mut session, 7);
    let encrypted = send_and_verify(&mut session, &mut peer);
    assert!(peer.decrypt(ALICE, MediaType::AUDIO, &encrypted).is_err(), "replay rejected");
    let mut tampered = session.encrypt_opus(OPUS).unwrap();
    tampered[1] ^= 1;
    assert!(peer.decrypt(ALICE, MediaType::AUDIO, &tampered).is_err());
}

#[test]
fn independent_peer_can_welcome_our_sender() {
    let host = Host::new();
    let (mut session, kp) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    let produced = peer
        .process_proposals(ProposalsOperationType::APPEND, &host.add(0, &kp), Some(&[ALICE, BOB]))
        .unwrap()
        .unwrap();
    peer.process_commit(&produced.commit).unwrap();
    transition(&mut session, 30, 8, &produced.welcome.unwrap());
    assert!(!session.ready());
    execute(&mut session, 8);
    send_and_verify(&mut session, &mut peer);
}

#[test]
fn member_add_and_remove_preserve_old_sender_until_execute() {
    let (host, mut session, mut peer) = established();
    let mut newcomer = host.peer(CAROL);
    let proposal = host.add_peer(1, &mut newcomer);
    let ours = append(&mut session, &proposal);
    peer.process_proposals(ProposalsOperationType::APPEND, &proposal, Some(&[ALICE, BOB, CAROL]))
        .unwrap();
    let (commit, welcome) = split_commit_welcome(&ours);
    transition(&mut session, 29, 2, commit);
    send_and_verify(&mut session, &mut peer);
    peer.process_commit(commit).unwrap();
    newcomer.process_welcome(welcome).unwrap();
    execute(&mut session, 2);
    send_and_verify(&mut session, &mut newcomer);

    let removed = session
        .group
        .as_ref()
        .unwrap()
        .members()
        .find(|m| identity(&m.credential).unwrap() == CAROL)
        .unwrap()
        .index;
    let proposal = host.remove(2, removed);
    append(&mut session, &proposal);
    let theirs =
        peer.process_proposals(ProposalsOperationType::APPEND, &proposal, None).unwrap().unwrap();
    transition(&mut session, 29, 3, &theirs.commit);
    send_and_verify(&mut session, &mut peer);
    peer.process_commit(&theirs.commit).unwrap();
    execute(&mut session, 3);
    let frame = send_and_verify(&mut session, &mut peer);
    assert!(newcomer.decrypt(ALICE, MediaType::AUDIO, &frame).is_err());
}

#[test]
fn revoked_proposals_do_not_enter_the_commit() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    append(&mut session, &host.add_peer(0, &mut peer));
    let reference = session
        .group
        .as_ref()
        .unwrap()
        .pending_proposals()
        .next()
        .unwrap()
        .proposal_reference_ref()
        .tls_serialize_detached()
        .unwrap();
    assert!(session.handle_binary(27, &op27_vl(1, reference)).unwrap().is_empty());
    assert!(session.own_commit.is_none());
}

#[test]
fn unknown_and_duplicate_members_are_rejected() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    session.handle_json(13, &json!({"user_id":BOB.to_string()})).unwrap();
    assert!(session.handle_binary(27, &op27(0, &host.add_peer(0, &mut peer))).is_err());
    let mut duplicate = host.peer(ALICE);
    assert!(session.handle_binary(27, &op27(0, &host.add_peer(0, &mut duplicate))).is_err());
}

#[test]
fn invalid_welcome_recovers_without_cleartext_and_is_bounded() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    for _ in 0..3 {
        let response = transition(&mut session, 30, 5, &[0, 0, 0]);
        assert!(matches!(&response[0], Outbound::Json(v) if v["op"] == 31));
        assert!(matches!(&response[1], Outbound::Binary(v) if v[0] == 26));
        assert!(!session.ready());
        execute(&mut session, 5);
        assert!(session.encrypt_opus(OPUS).is_err());
    }
    assert!(session.handle_binary(30, &[0, 5, 0, 0, 0]).is_err());
}

#[test]
fn stale_execute_does_not_reactivate_an_old_transition() {
    let (_, mut session, _) = established();
    prepare_v(&mut session, 0, 10).unwrap();
    prepare_v(&mut session, 0, 11).unwrap();
    execute(&mut session, 11);
    assert!(execute_res(&mut session, 10).is_err());
}

#[test]
fn welcome_validates_channel_external_sender_and_roster() {
    for invalid in ["channel", "sender", "roster"] {
        let host = Host::new();
        let (mut session, kp) = host.session(ALICE);
        let mut peer = host.peer(BOB);
        let produced = peer
            .process_proposals(ProposalsOperationType::APPEND, &host.add(0, &kp), None)
            .unwrap()
            .unwrap();
        match invalid {
            "channel" => session.channel += 1,
            "sender" => {
                session.external = Some(
                    ExternalSender::tls_deserialize_exact_bytes(&Host::new().external).unwrap(),
                )
            }
            _ => {
                session.expected.remove(&BOB);
            }
        }
        let response = transition(&mut session, 30, 9, &produced.welcome.unwrap());
        assert!(matches!(&response[0], Outbound::Json(v) if v["op"] == 31), "{invalid}");
        assert!(!session.ready());
    }
}

#[test]
fn explicit_downgrade_upgrade_and_solo_reset_do_not_reuse_keys() {
    let (host, mut session, mut peer) = established();
    prepare_v(&mut session, 0, 4).unwrap();
    send_and_verify(&mut session, &mut peer);
    execute(&mut session, 4);
    assert_eq!(session.encrypt_opus(OPUS).unwrap(), OPUS);
    announce_epoch(&mut session, 1, 1).unwrap();
    assert_eq!(session.active_version(), Some(0));
    prepare_v(&mut session, 1, 0).unwrap();
    assert_eq!(session.active_version(), Some(1));
    let first = session.encrypt_opus(OPUS).unwrap();
    assert!(prepare_v(&mut session, 1, 0).is_err());
    announce_epoch(&mut session, 1, 1).unwrap();
    prepare_v(&mut session, 1, 0).unwrap();
    assert_ne!(first, session.encrypt_opus(OPUS).unwrap());

    let mut late = Session::new(ALICE, CHANNEL).unwrap();
    late.negotiate(1).unwrap();
    assert!(!late.ready());
    late.handle_binary(25, &host.external).unwrap();
    assert!(late.group.is_some());
    assert!(execute_res(&mut late, 99).is_err());
}

#[test]
fn foreign_initial_commits_and_unknown_versions_are_not_accepted() {
    let (host, session, _) = established();
    let (mut fresh, _) = host.session(ALICE);
    let bytes =
        session.group.as_ref().unwrap().export_ratchet_tree().tls_serialize_detached().unwrap();
    assert!(fresh.commit(&bytes).is_err());
    let mut fresh = Session::new(ALICE, CHANNEL).unwrap();
    assert!(fresh.negotiate(2).is_err());
    assert!(fresh.encrypt_opus(OPUS).is_err());
}

#[test]
fn transitions_before_session_description_cannot_enable_audio() {
    for (opcode, data) in [
        (21, json!({"protocol_version":0,"transition_id":0})),
        (22, json!({"transition_id":0})),
        (24, json!({"protocol_version":1,"epoch":1})),
    ] {
        let mut session = Session::new(ALICE, CHANNEL).unwrap();
        assert!(session.handle_json(opcode, &data).is_err(), "opcode {opcode} accepted early");
        assert!(!session.ready());
        assert!(session.encrypt_opus(OPUS).is_err());
        session.negotiate(1).unwrap();
        assert!(!session.ready());
    }
}

#[test]
fn a_lone_member_sends_with_its_local_group_until_someone_joins() {
    let host = Host::new();
    let (mut crowded, _) = host.session(ALICE);
    assert!(!crowded.start_alone().unwrap());
    assert!(!crowded.ready());

    let mut session = Session::new(ALICE, CHANNEL).unwrap();
    assert!(!session.start_alone().unwrap());
    session.negotiate(1).unwrap();
    take_binary(session.handle_binary(25, &host.external).unwrap(), 26);
    assert!(session.start_alone().unwrap());
    assert!(session.ready());
    let alone = session.encrypt_opus(OPUS).unwrap();
    assert_ne!(alone, OPUS);
    assert!(!session.start_alone().unwrap());

    session.handle_json(11, &json!({"user_ids":[BOB.to_string()]})).unwrap();
    let mut peer = host.peer(BOB);
    let proposal = host.add_peer(0, &mut peer);
    welcome_peer(&mut session, &proposal, 3, &mut peer);
    execute(&mut session, 3);
    send_and_verify(&mut session, &mut peer);
    assert!(peer.decrypt(ALICE, MediaType::AUDIO, &alone).is_err());
}

#[test]
fn the_key_package_waits_for_the_external_sender() {
    let host = Host::new();
    let mut session = Session::new(ALICE, CHANNEL).unwrap();
    assert!(session.negotiate(1).unwrap().is_empty());
    let key_package = take_binary(session.handle_binary(25, &host.external).unwrap(), 26);
    assert!(KeyPackageIn::tls_deserialize_exact_bytes(&key_package).is_ok());
    assert!(session.handle_binary(25, &host.external).unwrap().is_empty());
}

#[test]
fn downgrade_and_reupgrade_on_unchanged_group_preserves_monotonic_nonces() {
    let (_, mut session, mut peer) = established();
    let first = send_and_verify(&mut session, &mut peer);

    prepare_v(&mut session, 0, 10).unwrap();
    execute(&mut session, 10);
    assert_eq!(session.active_version(), Some(0));
    assert_eq!(session.encrypt_opus(OPUS).unwrap(), OPUS);

    prepare_v(&mut session, 1, 11).unwrap();
    assert!(prepare_v(&mut session, 1, 12).is_err());
    execute(&mut session, 11);
    assert_eq!(session.active_version(), Some(1));

    let resumed = send_and_verify(&mut session, &mut peer);
    assert_ne!(first, resumed);
}

#[test]
fn more_than_32_abandoned_transitions_evict_fifo_instead_of_failing_session() {
    let (_, mut session, _) = established();
    for round in 1u16..=40 {
        announce_epoch(&mut session, 1, 1).unwrap();
        prepare_v(&mut session, 1, round).unwrap();
    }
    announce_epoch(&mut session, 1, 1).unwrap();
    assert_eq!(session.cancelled.len(), 32);
    assert!(execute_res(&mut session, 40).unwrap().is_empty());
}

#[test]
fn session_offset_encryption_handles_v0_v1_and_bounds() {
    let mut unready = Session::new(ALICE, CHANNEL).unwrap();
    let mut frame = vec![0u8; 16];
    assert!(matches!(unready.encrypt_opus_in_place_from(&mut frame, 12), Err(Error::NotReady)));

    let mut v0 = Session::new(ALICE, CHANNEL).unwrap();
    v0.negotiate(0).unwrap();
    let prefix = [0x80, 0x78, 0, 1, 0, 0, 3, 192, 0, 0, 0, 77];
    frame.clear();
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(OPUS);
    v0.encrypt_opus_in_place_from(&mut frame, prefix.len()).unwrap();
    assert_eq!(&frame[..prefix.len()], &prefix);
    assert_eq!(&frame[prefix.len()..], OPUS);
    let bad_v0 = frame.len() + 1;
    assert!(v0.encrypt_opus_in_place_from(&mut frame, bad_v0).is_err());

    let (_, mut v1, mut peer) = established();
    frame = Vec::with_capacity(1500);
    let ptr = frame.as_ptr();
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(OPUS);
    v1.encrypt_opus_in_place_from(&mut frame, prefix.len()).unwrap();
    assert_eq!(frame.as_ptr(), ptr);
    assert_eq!(&frame[..prefix.len()], &prefix);
    assert_ne!(&frame[prefix.len()..], OPUS);
    assert_eq!(peer.decrypt(ALICE, MediaType::AUDIO, &frame[prefix.len()..]).unwrap(), OPUS);
    let bad_v1 = frame.len() + 1;
    assert!(v1.encrypt_opus_in_place_from(&mut frame, bad_v1).is_err());
}

#[test]
fn rapid_downgrade_upgrade_churn_and_superseded_sender_salvage() {
    let (host, mut session, mut peer) = established();
    send_and_verify(&mut session, &mut peer);

    for (ver, id) in [(0, 10), (1, 11), (0, 12), (1, 13)] {
        prepare_v(&mut session, ver, id).unwrap();
    }
    assert!(prepare_v(&mut session, 1, 14).is_err());
    execute(&mut session, 13);
    assert_eq!(session.active_version(), Some(1));
    send_and_verify(&mut session, &mut peer);

    let mut newcomer = host.peer(CAROL);
    let proposal = host.add_peer(1, &mut newcomer);
    welcome_peer(&mut session, &proposal, 20, &mut newcomer);
    prepare_v(&mut session, 0, 21).unwrap();
    prepare_v(&mut session, 1, 22).unwrap();
    execute(&mut session, 22);
    assert_eq!(session.active_version(), Some(1));
    send_and_verify(&mut session, &mut newcomer);
}

#[test]
fn start_alone_from_transport_only_and_late_op21_reconciliation() {
    let host = Host::new();
    let mut session = Session::new(ALICE, CHANNEL).unwrap();
    session.negotiate(0).unwrap();
    assert_eq!(session.active_version(), Some(0));
    session.handle_binary(25, &host.external).unwrap();

    take_binary(announce_epoch(&mut session, 1, 1).unwrap(), 26);
    assert_eq!(session.active_version(), Some(0));
    assert!(session.start_alone().unwrap());
    assert_eq!(session.active_version(), Some(1));
    let first = session.encrypt_opus(OPUS).unwrap();
    assert_ne!(first, OPUS);

    let outbound = prepare_v(&mut session, 1, 5).unwrap();
    assert!(
        matches!(&outbound[0], Outbound::Json(v) if v["op"] == 23 && v["d"]["transition_id"] == 5)
    );
    execute(&mut session, 5);
    assert_eq!(session.active_version(), Some(1));
    assert_ne!(first, session.encrypt_opus(OPUS).unwrap());
    assert!(prepare_v(&mut session, 1, 6).is_err());
}

#[test]
fn transport_only_transitions_survive_op24_downgrade_and_group_reset() {
    let (_, mut session, _) = established();
    assert_eq!(session.active_version(), Some(1));

    prepare_v(&mut session, 0, 30).unwrap();
    announce_epoch(&mut session, 0, 0).unwrap();
    execute(&mut session, 30);
    assert_eq!(session.active_version(), Some(0));
    assert_eq!(session.encrypt_opus(OPUS).unwrap(), OPUS);

    let (_, mut session2, _) = established();
    prepare_v(&mut session2, 0, 31).unwrap();
    announce_epoch(&mut session2, 1, 1).unwrap();
    execute(&mut session2, 31);
    assert_eq!(session2.active_version(), Some(0));
}

#[test]
fn proposals_reject_pre_negotiation_and_validate_batches_atomically() {
    let host = Host::new();
    let mut unnegotiated = Session::new(ALICE, CHANNEL).unwrap();
    unnegotiated.handle_binary(25, &host.external).unwrap();
    assert!(unnegotiated.handle_binary(27, &[0, 0]).is_err());

    let (mut session, _) = host.session(ALICE);
    session.handle_json(13, &json!({"user_id":CAROL.to_string()})).unwrap();
    let mut bob = host.peer(BOB);
    let mut carol = host.peer(CAROL);
    let mut combined = read_vlbytes_exact(&host.add_peer(0, &mut bob)).unwrap().to_vec();
    combined.extend_from_slice(read_vlbytes_exact(&host.add_peer(0, &mut carol)).unwrap());
    assert!(session.handle_binary(27, &op27_vl(0, combined)).is_err());
    assert_eq!(session.group.as_ref().unwrap().pending_proposals().count(), 0);

    let mut bob2 = host.peer(BOB);
    append(&mut session, &host.add_peer(0, &mut bob2));
    assert_eq!(session.group.as_ref().unwrap().pending_proposals().count(), 1);
    let valid_ref = session
        .group
        .as_ref()
        .unwrap()
        .pending_proposals()
        .next()
        .unwrap()
        .proposal_reference_ref()
        .tls_serialize_detached()
        .unwrap();
    let mut bad_refs = valid_ref.clone();
    bad_refs.extend_from_slice(&valid_ref);
    assert!(session.handle_binary(27, &op27_vl(1, bad_refs)).is_err());
    assert_eq!(session.group.as_ref().unwrap().pending_proposals().count(), 1);
}

#[test]
fn external_sender_rotation_and_storage_zeroization() {
    let (host1, host2, host3) = (Host::new(), Host::new(), Host::new());
    let (mut session, _) = host1.session(ALICE);
    assert!(!session.provider.storage().values.read().unwrap().is_empty());

    let new_kp = take_binary(session.handle_binary(25, &host2.external).unwrap(), 26);
    assert!(KeyPackageIn::tls_deserialize_exact_bytes(&new_kp).is_ok());

    let mut bob = host2.peer(BOB);
    let proposal = host2.add_peer(0, &mut bob);
    welcome_peer(&mut session, &proposal, 1, &mut bob);
    execute(&mut session, 1);

    assert!(session.handle_binary(25, &host3.external).is_err());
    assert!(!session.provider.storage().values.read().unwrap().is_empty());
    zeroize_provider(&session.provider);
    assert!(session.provider.storage().values.read().unwrap().is_empty());
    announce_epoch(&mut session, 0, 0).unwrap();
    assert!(session.provider.storage().values.read().unwrap().is_empty());
    assert!(session.handle_binary(25, &host3.external).unwrap().is_empty());
}
