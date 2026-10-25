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
        let message = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            kp,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        VLBytes::new(message.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()
    }
    fn remove(&self, epoch: u64, index: LeafNodeIndex) -> Vec<u8> {
        let message = ExternalProposal::new_remove::<OpenMlsRustCrypto>(
            index,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        VLBytes::new(message.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()
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

fn raw_key_package(message: &[u8]) -> Vec<u8> {
    let message = MlsMessageIn::tls_deserialize_exact_bytes(message).unwrap();
    let MlsMessageBodyIn::KeyPackage(kp) = message.extract() else {
        panic!("key package must be an MLSMessage");
    };
    kp.tls_serialize_detached().unwrap()
}

fn append(session: &mut Session, proposals: &[u8]) -> Vec<u8> {
    let mut payload = vec![0];
    payload.extend(proposals);
    take_binary(session.handle_binary(27, &payload).unwrap(), 28)
}

fn split_commit_welcome(payload: &[u8]) -> (&[u8], &[u8]) {
    let (_, rest) = MlsMessageIn::tls_deserialize_bytes(payload).unwrap();
    (&payload[..payload.len() - rest.len()], rest)
}

fn transition(session: &mut Session, op: u8, id: u16, bytes: &[u8]) -> Vec<Outbound> {
    let mut payload = id.to_be_bytes().to_vec();
    payload.extend(bytes);
    session.handle_binary(op, &payload).unwrap()
}

fn execute(session: &mut Session, id: u16) {
    session.handle_json(22, &json!({"transition_id":id})).unwrap();
}

fn established() -> (Host, Session, DaveSession) {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    let proposal = host.add(0, &peer.create_key_package().unwrap());
    let payload = append(&mut session, &proposal);
    let (commit, welcome) = split_commit_welcome(&payload);
    transition(&mut session, 29, 1, commit);
    peer.process_welcome(welcome).unwrap();
    execute(&mut session, 1);
    (host, session, peer)
}

#[test]
fn own_initial_commit_is_interoperable_and_waits_for_execute() {
    let host = Host::new();
    let (mut session, key_package) = host.session(ALICE);
    assert!(!raw_key_package(&key_package).is_empty());
    let mut peer = host.peer(BOB);
    let proposal = host.add(0, &peer.create_key_package().unwrap());
    let payload = append(&mut session, &proposal);
    let (commit, welcome) = split_commit_welcome(&payload);
    let ready = transition(&mut session, 29, 7, commit);
    assert!(matches!(&ready[0],Outbound::Json(v) if v["op"] == 23 && v["d"]["transition_id"] == 7));
    peer.process_welcome(welcome).unwrap();
    assert!(matches!(session.encrypt_opus(OPUS), Err(Error::NotReady)));
    execute(&mut session, 7);
    let encrypted = session.encrypt_opus(OPUS).unwrap();
    assert_eq!(peer.decrypt(ALICE, MediaType::AUDIO, &encrypted).unwrap(), OPUS);
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
    let proposal = host.add(0, &raw_key_package(&kp));
    let produced = peer
        .process_proposals(ProposalsOperationType::APPEND, &proposal, Some(&[ALICE, BOB]))
        .unwrap()
        .unwrap();
    peer.process_commit(&produced.commit).unwrap();
    transition(&mut session, 30, 8, &produced.welcome.unwrap());
    assert!(!session.ready());
    execute(&mut session, 8);
    assert_eq!(
        peer.decrypt(ALICE, MediaType::AUDIO, &session.encrypt_opus(OPUS).unwrap()).unwrap(),
        OPUS
    );
}

#[test]
fn member_add_and_remove_preserve_old_sender_until_execute() {
    let (host, mut session, mut peer) = established();
    let mut newcomer = host.peer(CAROL);
    let proposal = host.add(1, &newcomer.create_key_package().unwrap());
    let ours = append(&mut session, &proposal);
    peer.process_proposals(ProposalsOperationType::APPEND, &proposal, Some(&[ALICE, BOB, CAROL]))
        .unwrap();
    let (commit, welcome) = split_commit_welcome(&ours);
    transition(&mut session, 29, 2, commit);
    // Receiver has not moved epochs yet, so this verifies the OLD sender remains in use.
    assert_eq!(
        peer.decrypt(ALICE, MediaType::AUDIO, &session.encrypt_opus(OPUS).unwrap()).unwrap(),
        OPUS
    );
    peer.process_commit(commit).unwrap();
    newcomer.process_welcome(welcome).unwrap();
    execute(&mut session, 2);
    let frame = session.encrypt_opus(OPUS).unwrap();
    assert_eq!(newcomer.decrypt(ALICE, MediaType::AUDIO, &frame).unwrap(), OPUS);
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
    // Exercise another member's winning commit, not just the local commit path.
    transition(&mut session, 29, 3, &theirs.commit);
    assert_eq!(
        peer.decrypt(ALICE, MediaType::AUDIO, &session.encrypt_opus(OPUS).unwrap()).unwrap(),
        OPUS
    );
    peer.process_commit(&theirs.commit).unwrap();
    execute(&mut session, 3);
    let frame = session.encrypt_opus(OPUS).unwrap();
    assert_eq!(peer.decrypt(ALICE, MediaType::AUDIO, &frame).unwrap(), OPUS);
    assert!(newcomer.decrypt(ALICE, MediaType::AUDIO, &frame).is_err());
}

#[test]
fn revoked_proposals_do_not_enter_the_commit() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    let proposal = host.add(0, &peer.create_key_package().unwrap());
    append(&mut session, &proposal);
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
    let mut revoke = vec![1];
    revoke.extend(VLBytes::new(reference).tls_serialize_detached().unwrap());
    assert!(session.handle_binary(27, &revoke).unwrap().is_empty());
    assert!(session.own_commit.is_none());
}

#[test]
fn unknown_and_duplicate_members_are_rejected() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    let mut peer = host.peer(BOB);
    session.handle_json(13, &json!({"user_id":BOB.to_string()})).unwrap();
    let proposal = host.add(0, &peer.create_key_package().unwrap());
    let mut bytes = vec![0];
    bytes.extend(proposal);
    assert!(session.handle_binary(27, &bytes).is_err());
    let mut duplicate = host.peer(ALICE);
    let mut bytes = vec![0];
    bytes.extend(host.add(0, &duplicate.create_key_package().unwrap()));
    assert!(session.handle_binary(27, &bytes).is_err());
}

#[test]
fn invalid_welcome_recovers_without_cleartext_and_is_bounded() {
    let host = Host::new();
    let (mut session, _) = host.session(ALICE);
    for _ in 0..3 {
        let response = transition(&mut session, 30, 5, &[0, 0, 0]);
        assert!(matches!(&response[0],Outbound::Json(v) if v["op"] == 31));
        assert!(matches!(&response[1],Outbound::Binary(v) if v[0] == 26));
        assert!(!session.ready());
        // A gateway transition timeout must not abort the recovery or enable clear audio.
        session.handle_json(22, &json!({"transition_id":5})).unwrap();
        assert!(session.encrypt_opus(OPUS).is_err());
    }
    assert!(session.handle_binary(30, &[0, 5, 0, 0, 0]).is_err());
}

#[test]
fn stale_execute_does_not_reactivate_an_old_transition() {
    let (_, mut session, _) = established();
    session.handle_json(21, &json!({"protocol_version":0,"transition_id":10})).unwrap();
    session.handle_json(21, &json!({"protocol_version":0,"transition_id":11})).unwrap();
    execute(&mut session, 11);
    assert!(session.handle_json(22, &json!({"transition_id":10})).is_err());
}

#[test]
fn welcome_validates_channel_external_sender_and_roster() {
    for invalid in ["channel", "sender", "roster"] {
        let host = Host::new();
        let (mut session, kp) = host.session(ALICE);
        let mut peer = host.peer(BOB);
        let proposal = host.add(0, &raw_key_package(&kp));
        let produced = peer
            .process_proposals(ProposalsOperationType::APPEND, &proposal, None)
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
        assert!(matches!(&response[0],Outbound::Json(v) if v["op"] == 31), "{invalid}");
        assert!(!session.ready());
    }
}

#[test]
fn explicit_downgrade_upgrade_and_solo_reset_do_not_reuse_keys() {
    let (host, mut session, mut peer) = established();
    session.handle_json(21, &json!({"protocol_version":0,"transition_id":4})).unwrap();
    assert_eq!(
        peer.decrypt(ALICE, MediaType::AUDIO, &session.encrypt_opus(OPUS).unwrap()).unwrap(),
        OPUS
    );
    execute(&mut session, 4);
    assert_eq!(session.encrypt_opus(OPUS).unwrap(), OPUS);
    session.handle_json(24, &json!({"protocol_version":1,"epoch":1})).unwrap();
    assert_eq!(session.active_version(), Some(0));
    session.handle_json(21, &json!({"protocol_version":1,"transition_id":0})).unwrap();
    assert_eq!(session.active_version(), Some(1));
    let first = session.encrypt_opus(OPUS).unwrap();
    assert!(session.handle_json(21, &json!({"protocol_version":1,"transition_id":0})).is_err());
    session.handle_json(24, &json!({"protocol_version":1,"epoch":1})).unwrap();
    session.handle_json(21, &json!({"protocol_version":1,"transition_id":0})).unwrap();
    assert_ne!(first, session.encrypt_opus(OPUS).unwrap());
    // External sender may arrive before or after Session Description.
    let mut late = Session::new(ALICE, CHANNEL).unwrap();
    late.negotiate(1).unwrap();
    assert!(!late.ready());
    late.handle_binary(25, &host.external).unwrap();
    assert!(late.group.is_some());
    assert!(late.handle_json(22, &json!({"transition_id":99})).is_err());
}

#[test]
fn foreign_initial_commits_and_unknown_versions_are_not_accepted() {
    let (host, session, _) = established();
    let (mut fresh, _) = host.session(ALICE);
    // Any foreign public message is invalid before joining an established group.
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
        assert!(
            session.handle_json(opcode, &data).is_err(),
            "opcode {opcode} was accepted before the selected DAVE version"
        );
        assert!(!session.ready());
        assert!(session.encrypt_opus(OPUS).is_err());
        session.negotiate(1).unwrap();
        assert!(!session.ready(), "DAVE v1 must await its own valid transition");
    }
}
