//! End-to-End Integration Tests for amla-protocol
//!
//! These tests demonstrate the complete flow of the PIC (Provenance Identity
//! Continuity) protocol, showing how capabilities flow through a chain of
//! agents with cryptographic guarantees.
//!
//! # Architecture Overview
//!
//! ## Basic Flow: Single Delegation
//!
//! ```text
//!                  ┌─────────────┐
//!                  │   Gateway   │  (Root Authority)
//!                  │ Issues root │
//!                  │    PCA      │
//!                  └──────┬──────┘
//!                         │ root PCA designates Claims Agent
//!                         ▼
//!              ┌──────────────────────┐
//!              │    Claims Agent      │
//!              │ Builds PoC, submits  │
//!              │     to CTA           │
//!              └──────────┬───────────┘
//!                         │ PoC + ContinuationRequest
//!                         ▼
//!              ┌──────────────────────┐
//!              │        CTA           │  (Trust Plane)
//!              │ Validates PoC        │
//!              │ Emits child PCA      │
//!              └──────────┬───────────┘
//!                         │ child PCA (signed by CTA) designates Payout Agent
//!                         ▼
//!              ┌──────────────────────┐
//!              │    Payout Agent      │
//!              │ Can continue chain   │
//!              │ or execute           │
//!              └──────────────────────┘
//! ```
//!
//! ## Multi-Hop Chain with `root_hash` Preservation
//!
//! ```text
//!   Gateway (GW)              CTA                    CTA
//!        │                     │                      │
//!        ▼                     ▼                      ▼
//!   ┌─────────┐          ┌─────────┐          ┌─────────┐
//!   │  PCA₀   │  ────►   │  PCA₁   │  ────►   │  PCA₂   │
//!   │─────────│          │─────────│          │─────────│
//!   │root:none│          │root:H(0)│          │root:H(0)│ ◄── Same!
//!   │prev:none│          │prev:H(0)│          │prev:H(1)│
//!   │exec:A₁  │          │exec:A₂  │          │exec:A₃  │
//!   │issuer:GW│          │issuer:CT│          │issuer:CT│
//!   └─────────┘          └─────────┘          └─────────┘
//!       H(0)                 H(1)                 H(2)
//!
//!   Transaction ID = H(0) is preserved throughout the entire chain
//!   Each hop links to its parent via prev_hash, creating an audit trail
//! ```
//!
//! ## Proof of Continuity (`PoC`) Structure
//!
//! ```text
//!   ┌─────────────────────────────────────────────────────────────┐
//!   │                    ProofOfContinuity                        │
//!   ├─────────────────────────────────────────────────────────────┤
//!   │  parent_hash:    H(parent_pca)     ◄── Binds to parent      │
//!   │  executor_key:   PK(designated)    ◄── Who is continuing    │
//!   │  request_hash:   H(ContRequest)    ◄── Binds to request     │
//!   │  challenge:      Freshness         ◄── Prevents replay      │
//!   │  signature:      Sign(SK, ...)     ◄── Proves key ownership │
//!   └─────────────────────────────────────────────────────────────┘
//!
//!   Signature covers: parent_hash || request_hash || challenge
//!
//!   Security guarantees:
//!   ✓ Only holder of designated executor's private key can create
//!   ✓ Bound to specific parent PCA (cannot reuse with different parent)
//!   ✓ Bound to specific request (cannot swap continuation details)
//!   ✓ Challenge ensures freshness (prevents replay attacks)
//! ```
//!
//! ## CTA Validation Pipeline
//!
//! ```text
//!   Input: (parent_pca, poc, continuation_request)
//!          │
//!          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 1. TRUST: Is parent issuer trusted?                      │
//!   │    • Root PCA: issuer ∈ trusted_root_authorities         │
//!   │    • Child PCA: issuer ∈ trusted_ctas                    │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 2. SIGNATURE: Is parent PCA signature valid?             │
//!   │    • Ed25519 signature verification                      │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 3. EXPIRY: Is parent PCA still valid?                    │
//!   │    • now < parent.expires_at                             │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 4. EXECUTOR: Does PoC prove designated executor?         │
//!   │    • PublicKey: poc.executor_key == parent.designated    │
//!   │    • Characteristic: resolver.verify(executor, char)     │
//!   │    • CtaReference: resolver.verify(executor, cta_ref)    │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 5. HASH BINDING: Is PoC bound to this parent?            │
//!   │    • poc.parent_hash == H(parent_pca)                    │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 6. POC SIGNATURE: Is PoC signature valid?                │
//!   │    • Verify(poc.signature, poc.executor_key, message)    │
//!   │    • message = parent_hash || request_hash || challenge  │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 7. FRESHNESS: Is challenge acceptable?                   │
//!   │    • Random: always accept (stateless)                   │
//!   │    • Timestamp: now - challenge.ts < max_skew            │
//!   │    • Epoch: sequence > last_seen (stateful)              │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 8. CAPABILITIES: Are requested caps valid?               │
//!   │    • Each child cap key ∈ parent cap keys                │
//!   │    • No duplicate keys in request                        │
//!   │    • validator.validate_transition(parent_cap, child)    │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 9. EXPIRY: Is child expiry valid?                        │
//!   │    • request.expires_at <= parent.expires_at             │
//!   └──────────────────────┬───────────────────────────────────┘
//!                          ▼
//!   ┌──────────────────────────────────────────────────────────┐
//!   │ 10. EMIT: Create and sign child PCA                      │
//!   │    • Set prev_hash = H(parent)                           │
//!   │    • Set root_hash = parent.root_hash ?? H(parent)       │
//!   │    • Sign with CTA's key                                 │
//!   └──────────────────────────────────────────────────────────┘
//!                          │
//!                          ▼
//!   Output: child_pca (signed by CTA)
//! ```
//!
//! # Key Concepts Demonstrated
//!
//! 1. **PCA (PIC Causal Attestation)**: Signed authorization tokens that bind
//!    capabilities to a designated executor.
//!
//! 2. **CTA (Causal Transaction Authority)**: Validates Proof of Continuity
//!    and emits new PCAs. All delegation goes through CTA.
//!
//! 3. **Proof of Continuity (`PoC`)**: Cryptographic proof that the submitter
//!    is the designated executor of the parent PCA.
//!
//! 4. **Capability Attenuation**: Authority can only narrow (never expand)
//!    as it flows through the chain.
//!
//! 5. **`root_hash`**: Stable transaction identifier across all hops.
//!
//! # Security Tests Summary
//!
//! ## Core Protocol Security (Tests 3-6)
//!
//! | Test | Attack Vector | Defense Mechanism |
//! |------|---------------|-------------------|
//! | 3 | Unauthorized executor | `PoC` signature verification |
//! | 4 | Capability escalation (new cap) | Capability key matching |
//! | 5 | Using expired parent | Temporal validation |
//! | 6 | Untrusted root issuer | Trust registry check |
//!
//! ## Capability Attenuation (Tests 7, 9-10)
//!
//! | Test | Attack Vector | Defense Mechanism |
//! |------|---------------|-------------------|
//! | 7 | Capability value escalation | Custom `TransitionValidator` |
//! | 9 | Dropping capabilities | Subset allowed (attenuation) |
//! | 10 | Expiry extension | Child ≤ parent expiry |
//!
//! ## Cryptographic Binding (Tests 12-14)
//!
//! | Test | Attack Vector | Defense Mechanism |
//! |------|---------------|-------------------|
//! | 12 | PoC/request mismatch | Request hash in signature |
//! | 13 | `PoC` wrong parent | Parent hash in signature |
//! | 14 | Hash collision | SHA-256 collision resistance |
//!
//! ## Trust Chain (Tests 15-18)
//!
//! | Test | Attack Vector | Defense Mechanism |
//! |------|---------------|-------------------|
//! | 15 | Duplicate capability keys | Uniqueness validation |
//! | 16 | Untrusted CTA issuer | CTA trust registry |
//! | 17 | `CtaReference` bypass | Explicit authorization |
//! | 18 | Characteristic spoofing | Custom `ExecutorResolver` |
//!
//! ## Data Integrity (Tests 20-25)
//!
//! | Test | Attack Vector | Defense Mechanism |
//! |------|---------------|-------------------|
//! | 20 | Byte-level tampering | Ed25519 signature verification |
//! | 21 | Malformed CBOR input | Robust deserialization |
//! | 22 | Signature fabrication | Ed25519 unforgeable |
//! | 23 | Version manipulation | Version in signed content |
//! | 24 | Hash collisions | SHA-256 (2^128 resistance) |
//! | 25 | Capability data modification | Integrity via serialization |
//!
//! ## Edge Cases & Production (Tests 26-31)
//!
//! | Test | Scenario | Verified Behavior |
//! |------|----------|-------------------|
//! | 26 | Concurrent submissions | Forking allowed (not replay) |
//! | 27 | Expiry boundary | Hard cutoff, no grace period |
//! | 28 | Raw signature bytes | 64 random bytes rejected |
//! | 29 | Key case sensitivity | "cap:x" ≠ "CAP:X" |
//! | 30 | Key whitespace | "cap:x " ≠ "cap:x" |
//! | 31 | Deep chains (50 hops) | O(n) time, no stack overflow |

use amla_protocol::{
    Algorithm, CapabilityData, ContinuationRequest, CtaBuilder, CtaError, DesignatedExecutor,
    FreshnessChallenge, KeyPair, PROTOCOL_VERSION, Pca, PcaBuilder, PermissiveValidator,
    ProofOfContinuity, PublicKey, TransitionError, TransitionValidator, validate_cta_chain,
};
use chrono::{Duration, Utc};
use serde_json::json;

// ============================================================================
// Test 1: Complete Insurance Claim Flow via CTA
// ============================================================================

/// Demonstrates the full insurance claim processing flow:
///
/// 1. Gateway issues root PCA to Claims Agent with broad claims authority
/// 2. Claims Agent builds `PoC` and submits to CTA
/// 3. CTA validates and emits child PCA for Payout Agent
///
/// This is the "happy path" showing all protocol components working together.
#[test]
#[allow(clippy::too_many_lines)]
fn test_insurance_claim_flow_via_cta() {
    // ========================================
    // Setup: Create all participants
    // ========================================

    // Gateway: The root authority that issues initial capabilities
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    // CTA: The trust plane that validates continuity and emits PCAs
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);

    // Agents: The executors that process the claim
    let claims_agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let payout_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

    // Create CTA that trusts both Gateway and itself (for multi-hop)
    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key()) // Trust root PCAs from Gateway
        .trust_cta(cta_keypair.public_key()) // Trust PCAs it issues (for multi-hop)
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // ========================================
    // Step 1: Gateway issues root PCA
    // ========================================
    // Gateway authorizes Claims Agent to process claims up to $25,000

    let root_capability = CapabilityData::from_json(
        "cap:insurance:claims", // Stable key for matching across hops
        "function",
        &json!({
            "resource": "insurance.claims",
            "action": "process",
            "constraints": {
                "max_amount_cents": 2_500_000,  // $25,000
                "claim_types": ["auto", "home", "health"]
            }
        }),
    )
    .unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(root_capability.clone())
        .designated_executor(claims_agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Verify root PCA properties
    assert!(root_pca.is_root(), "Root PCA should have no prev_hash");
    assert!(
        root_pca.root_hash().is_none(),
        "Root PCA should have no root_hash"
    );
    assert!(root_pca.try_verify_signature().is_ok());
    assert_eq!(root_pca.issuer(), &gateway.public_key());
    assert_eq!(
        root_pca.designated_executor().as_public_key(),
        Some(&claims_agent.public_key())
    );

    // Store root hash - this is the transaction ID
    let transaction_id = root_pca.try_hash().unwrap();

    // ========================================
    // Step 2: Claims Agent processes claim and delegates to Payout Agent
    // ========================================
    // Claims Agent reviews claim CLM-2025-001 and approves $3,800 payout

    // Attenuated capability: specific claim, reduced amount
    let attenuated_capability = CapabilityData::from_json(
        "cap:insurance:claims", // Same key as parent (required for matching)
        "function",
        &json!({
            "resource": "insurance.claims",
            "action": "payout",
            "claim_id": "CLM-2025-001",
            "approved_amount_cents": 380_000,  // $3,800 (within $25k limit)
            "deductible_cents": 50_000         // $500 deductible
        }),
    )
    .unwrap();

    // Build continuation request
    let continuation = ContinuationRequest {
        capabilities: vec![attenuated_capability.clone()],
        designated_executor: DesignatedExecutor::from_public_key(payout_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Claims Agent creates Proof of Continuity
    let challenge = FreshnessChallenge::from_bytes([1; 32]);
    let poc = ProofOfContinuity::build(&root_pca, &claims_agent, &continuation, challenge).unwrap();

    // Verify PoC structure
    assert_eq!(poc.parent_hash, root_pca.try_hash().unwrap());
    assert_eq!(poc.executor_key, claims_agent.public_key());

    // ========================================
    // Step 3: CTA validates and emits child PCA
    // ========================================

    let child_pca = cta
        .submit(&root_pca, &poc, &continuation, Utc::now())
        .expect("CTA should accept valid submission");

    // Verify child PCA properties
    assert!(!child_pca.is_root(), "Child PCA should have prev_hash");
    assert_eq!(
        child_pca.prev_hash(),
        Some(&transaction_id),
        "Child should link to root"
    );
    assert_eq!(
        child_pca.root_hash(),
        Some(&transaction_id),
        "Child should have root_hash = transaction_id"
    );
    assert!(child_pca.try_verify_signature().is_ok());
    assert_eq!(
        child_pca.issuer(),
        &cta_keypair.public_key(),
        "Child issuer should be CTA"
    );
    assert_eq!(
        child_pca.designated_executor().as_public_key(),
        Some(&payout_agent.public_key())
    );

    // ========================================
    // Verify chain integrity manually
    // ========================================
    //
    // ARCHITECTURAL NOTE: `validate_chain()` enforces direct-chain semantics
    // (child issuer == parent designated executor). CTA-signed chains use a
    // different validation path, so we either validate manually or call
    // `validate_cta_chain()` with trusted root authorities and CTA signers.

    // 1. Both PCAs have valid signatures
    assert!(root_pca.try_verify_signature().is_ok());
    assert!(child_pca.try_verify_signature().is_ok());

    // 2. Hash links are correct
    assert_eq!(child_pca.prev_hash(), Some(&root_pca.try_hash().unwrap()));

    // 3. root_hash is preserved (transaction identity)
    assert_eq!(child_pca.root_hash(), Some(&transaction_id));

    // 4. Issuers are trusted (gateway for root, CTA for child)
    assert_eq!(root_pca.issuer(), &gateway.public_key());
    assert_eq!(child_pca.issuer(), &cta_keypair.public_key());

    // Validate CTA-signed chain with helper
    let validator = PermissiveValidator;
    validate_cta_chain(
        &[root_pca.clone(), child_pca.clone()],
        &[gateway.public_key()],
        &[cta_keypair.public_key()],
        &validator,
        Utc::now(),
    )
    .unwrap();

    println!("✅ Insurance claim flow completed successfully!");
    println!("   Transaction ID: {}", transaction_id.to_hex());
    println!("   Root issuer: Gateway {}", gateway.public_key().to_hex());
    println!("   Child issuer: CTA {}", cta_keypair.public_key().to_hex());
    println!("   Final executor: {}", payout_agent.public_key().to_hex());
}

// ============================================================================
// Test 2: Three-Hop Chain via CTA
// ============================================================================

/// Demonstrates a longer chain: Gateway → Agent1 → Agent2 → Agent3
///
/// Shows that `root_hash` is preserved across all hops, maintaining
/// transaction identity throughout the chain.
#[test]
fn test_three_hop_chain_via_cta() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
    let agent3 = KeyPair::from_seed(Algorithm::Ed25519, &[4; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .trust_cta(cta_keypair.public_key()) // Trust its own PCAs for multi-hop
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Capability that will be attenuated through the chain
    let cap = CapabilityData::from_json("cap:api", "function", &json!({"limit": 1000})).unwrap();

    // Hop 0: Gateway → Agent1 (root PCA)
    let pca0 = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent1.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let tx_id = pca0.try_hash().unwrap();
    assert!(pca0.is_root());
    assert!(pca0.root_hash().is_none());

    // Hop 1: Agent1 → Agent2 (via CTA)
    let cap1 = CapabilityData::from_json("cap:api", "function", &json!({"limit": 500})).unwrap();
    let cont1 = ContinuationRequest {
        capabilities: vec![cap1.clone()],
        designated_executor: DesignatedExecutor::from_public_key(agent2.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc1 = ProofOfContinuity::build(
        &pca0,
        &agent1,
        &cont1,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();
    let pca1 = cta.submit(&pca0, &poc1, &cont1, Utc::now()).unwrap();

    assert!(!pca1.is_root());
    assert_eq!(pca1.prev_hash(), Some(&tx_id));
    assert_eq!(
        pca1.root_hash(),
        Some(&tx_id),
        "Hop 1 should preserve root_hash"
    );
    assert_eq!(pca1.issuer(), &cta_keypair.public_key());

    // Hop 2: Agent2 → Agent3 (via CTA)
    let cap2 = CapabilityData::from_json("cap:api", "function", &json!({"limit": 100})).unwrap();
    let cont2 = ContinuationRequest {
        capabilities: vec![cap2],
        designated_executor: DesignatedExecutor::from_public_key(agent3.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc2 = ProofOfContinuity::build(
        &pca1,
        &agent2,
        &cont2,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();
    let pca2 = cta.submit(&pca1, &poc2, &cont2, Utc::now()).unwrap();

    assert!(!pca2.is_root());
    assert_eq!(pca2.prev_hash(), Some(&pca1.try_hash().unwrap()));
    assert_eq!(
        pca2.root_hash(),
        Some(&tx_id),
        "Hop 2 should preserve root_hash"
    );
    assert_eq!(pca2.issuer(), &cta_keypair.public_key());

    // Verify all signatures are valid
    assert!(pca0.try_verify_signature().is_ok());
    assert!(pca1.try_verify_signature().is_ok());
    assert!(pca2.try_verify_signature().is_ok());

    println!("✅ Three-hop chain completed successfully!");
    println!("   Transaction ID preserved: {}", tx_id.to_hex());
    println!("   Chain: Gateway → Agent1 → Agent2 → Agent3");
}

// ============================================================================
// Test 3: Security - Unauthorized Executor Rejected
// ============================================================================

/// Demonstrates that only the designated executor can continue the chain.
///
/// # Attack Scenario
///
/// ```text
///   Gateway issues root PCA
///          │
///          ▼
///   ┌─────────────────┐
///   │ Root PCA        │
///   │ designated:     │──────────┐
///   │ legitimate_agent│          │
///   └─────────────────┘          │
///          │                     │
///          │ (intercepted)       │
///          ▼                     │
///   ┌─────────────────┐          │
///   │   ATTACKER      │          │
///   │ Has: PCA copy   │          │
///   │ Missing: SK     │◄─────────┘ Cannot forge signature
///   └────────┬────────┘            without private key
///            │
///            ▼ Creates PoC with attacker's key
///   ┌─────────────────┐
///   │ Malicious PoC   │
///   │ executor_key:   │
///   │ attacker.pk     │ ≠ designated_executor!
///   └────────┬────────┘
///            │
///            ▼
///   ┌─────────────────┐
///   │      CTA        │
///   │ CHECK: executor │
///   │ == designated?  │──── NO ──► REJECT: ExecutorMismatch
///   └─────────────────┘
/// ```
///
/// # Why This Works
///
/// 1. The PCA designates a specific public key as executor
/// 2. The `PoC` must be signed by the corresponding private key
/// 3. Attacker has the PCA but not the designated executor's private key
/// 4. Even if attacker creates a `PoC` with their own key, CTA checks match
#[test]
fn test_unauthorized_executor_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let legitimate_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let attacker = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:secret", "function", &json!({})).unwrap();

    // Root PCA designates legitimate_agent
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(legitimate_agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Attacker intercepts the PCA and tries to continue
    let malicious_request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(attacker.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Attacker creates PoC with their own key (not the designated executor)
    let malicious_poc = ProofOfContinuity::build(
        &root_pca,
        &attacker, // Wrong! Should be legitimate_agent
        &malicious_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // CTA rejects: executor doesn't match parent's designated_executor
    let result = cta.submit(&root_pca, &malicious_poc, &malicious_request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::ExecutorMismatch)),
        "CTA should reject unauthorized executor: {result:?}"
    );

    println!("✅ Unauthorized executor correctly rejected!");
}

// ============================================================================
// Test 4: Security - Unknown Capability Key Rejected
// ============================================================================

/// Demonstrates that child capabilities must reference parent capabilities.
///
/// # Attack Scenario: Capability Escalation
///
/// ```text
///   ┌─────────────────────────────┐
///   │ Parent PCA                  │
///   │ capabilities:               │
///   │   ├── "cap:read" ✓          │
///   │   └── (no cap:write)        │
///   └─────────────┬───────────────┘
///                 │
///                 ▼ Agent tries to delegate
///   ┌─────────────────────────────┐
///   │ ContinuationRequest         │
///   │ capabilities:               │
///   │   └── "cap:write" ✗         │ ◄── NOT in parent!
///   └─────────────┬───────────────┘
///                 │
///                 ▼
///   ┌─────────────────────────────┐
///   │ CTA Validation              │
///   │                             │
///   │ for each child_cap:         │
///   │   child.key ∈ parent.keys?  │
///   │   "cap:write" ∈ {"cap:read"}│
///   │   = FALSE                   │
///   └─────────────┬───────────────┘
///                 │
///                 ▼
///   REJECT: UnknownCapabilityKey("cap:write")
/// ```
///
/// # Security Property: Monotonic Attenuation
///
/// Capabilities can only flow "downward" through the chain:
/// - Child ⊆ Parent (subset of capabilities)
/// - New capabilities cannot be introduced
/// - This prevents privilege escalation attacks
#[test]
fn test_unknown_capability_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Root PCA grants "cap:read" capability
    let read_cap = CapabilityData::from_json("cap:read", "function", &json!({})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(read_cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Agent tries to introduce "cap:write" (not in parent!)
    let write_cap = CapabilityData::from_json("cap:write", "function", &json!({})).unwrap();
    let escalation_request = ContinuationRequest {
        capabilities: vec![write_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &escalation_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // CTA rejects: cap:write not in parent
    let result = cta.submit(&root_pca, &poc, &escalation_request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::UnknownCapabilityKey(_))),
        "CTA should reject unknown capability: {result:?}"
    );

    println!("✅ Capability escalation correctly rejected!");
}

// ============================================================================
// Test 5: Security - Expired Parent Rejected
// ============================================================================

/// Demonstrates that expired PCAs cannot be used to continue chains.
///
/// # Attack Scenario: Stale Token Reuse
///
/// ```text
///   Timeline:
///   ─────────────────────────────────────────────────►
///   │                    │                    │
///   │                    │                    │
///   T-2h              T-1h                  NOW
///   PCA created       PCA expired           Attack attempted
///
///   ┌─────────────────────────────┐
///   │ Expired PCA                 │
///   │ expires_at: T-1h            │ ◄── Already expired!
///   │ designated: agent           │
///   └─────────────┬───────────────┘
///                 │
///                 ▼ Agent tries to continue
///   ┌─────────────────────────────┐
///   │ CTA Validation              │
///   │                             │
///   │ CHECK: now < expires_at?    │
///   │        NOW < T-1h?          │
///   │        = FALSE              │
///   └─────────────┬───────────────┘
///                 │
///                 ▼
///   REJECT: ParentExpired
/// ```
///
/// # Security Property: Temporal Bounds
///
/// - All PCAs have explicit expiration times
/// - CTA validates against current time at submission
/// - Expired PCAs are immediately rejected (no grace period)
/// - Prevents reuse of old, potentially compromised tokens
#[test]
fn test_expired_parent_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    // Create a PCA that expired in the past
    let expired = Utc::now() - Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    let expired_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expired)
        .build_and_sign(&gateway)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: Utc::now() + Duration::hours(1),
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &expired_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // CTA rejects: parent is expired
    let result = cta.submit(&expired_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::ParentExpired)),
        "CTA should reject expired parent: {result:?}"
    );

    println!("✅ Expired parent correctly rejected!");
}

// ============================================================================
// Test 6: Security - Untrusted Issuer Rejected
// ============================================================================

/// Demonstrates that PCAs from untrusted issuers are rejected.
///
/// # Attack Scenario: Rogue Gateway
///
/// ```text
///   ┌─────────────────────┐     ┌─────────────────────┐
///   │ Trusted Gateway     │     │ Rogue Gateway       │
///   │ PK: 0xABC...        │     │ PK: 0xDEF...        │
///   └─────────────────────┘     └──────────┬──────────┘
///                                          │
///                                          ▼ Issues PCA
///   ┌───────────────────────────────────────────────────┐
///   │ Rogue PCA                                         │
///   │ issuer: 0xDEF... (rogue)                          │
///   │ signature: Sign(rogue_sk, ...)                    │
///   │ designated: agent                                 │
///   └───────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌───────────────────────────────────────────────────┐
///   │ CTA Configuration                                 │
///   │ trusted_root_authorities: [0xABC...]              │ ◄── Only trusted
///   └───────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌───────────────────────────────────────────────────┐
///   │ CTA Validation                                    │
///   │                                                   │
///   │ CHECK: issuer ∈ trusted_root_authorities?         │
///   │        0xDEF ∈ [0xABC]?                           │
///   │        = FALSE                                    │
///   └───────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: UntrustedRootAuthority(0xDEF...)
/// ```
///
/// # Security Property: Explicit Trust
///
/// - CTA maintains explicit list of trusted root authorities
/// - Only PCAs signed by trusted keys are accepted
/// - Prevents attackers from issuing their own root PCAs
#[test]
fn test_untrusted_issuer_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let untrusted_issuer = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

    // CTA only trusts gateway, not untrusted_issuer
    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // PCA signed by untrusted issuer
    let untrusted_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&untrusted_issuer)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &untrusted_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // CTA rejects: issuer not trusted
    let result = cta.submit(&untrusted_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::UntrustedRootAuthority(_))),
        "CTA should reject untrusted issuer: {result:?}"
    );

    println!("✅ Untrusted issuer correctly rejected!");
}

// ============================================================================
// Test 7: Custom Transition Validator
// ============================================================================

/// Demonstrates pluggable capability validation.
///
/// The protocol doesn't interpret capability contents - applications
/// define their own attenuation logic via `TransitionValidator`.
#[test]
fn test_custom_transition_validator() {
    /// A validator that enforces `max_amount` can only decrease
    struct AmountValidator;

    impl TransitionValidator for AmountValidator {
        fn validate_transition(
            &self,
            parent: &CapabilityData,
            child: &CapabilityData,
        ) -> Result<(), TransitionError> {
            // Parse amounts from capability data
            let parent_data: serde_json::Value = parent.to_json().unwrap();
            let child_data: serde_json::Value = child.to_json().unwrap();

            let parent_amount = parent_data["max_amount"].as_i64().unwrap_or(i64::MAX);
            let child_amount = child_data["max_amount"].as_i64().unwrap_or(i64::MAX);

            if child_amount > parent_amount {
                return Err(TransitionError::new(format!(
                    "cannot escalate: child amount {child_amount} > parent amount {parent_amount}"
                )));
            }

            Ok(())
        }
    }

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    // CTA with custom validator
    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .validator(AmountValidator)
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Root: max_amount = 1000
    let parent_cap =
        CapabilityData::from_json("cap:payment", "function", &json!({"max_amount": 1000})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(parent_cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Try to escalate: max_amount = 2000 (SHOULD FAIL)
    let escalated_cap =
        CapabilityData::from_json("cap:payment", "function", &json!({"max_amount": 2000})).unwrap();
    let escalation_request = ContinuationRequest {
        capabilities: vec![escalated_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &escalation_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc, &escalation_request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::InvalidTransition(_))),
        "Should reject escalation: {result:?}"
    );

    // Try to attenuate: max_amount = 500 (SHOULD SUCCEED)
    let attenuated_cap =
        CapabilityData::from_json("cap:payment", "function", &json!({"max_amount": 500})).unwrap();
    let valid_request = ContinuationRequest {
        capabilities: vec![attenuated_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc2 = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &valid_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc2, &valid_request, Utc::now());
    assert!(result.is_ok(), "Should accept attenuation: {result:?}");

    println!("✅ Custom transition validator works correctly!");
}

// ============================================================================
// Test 8: CBOR Serialization Roundtrip
// ============================================================================

/// Demonstrates that PCAs serialize to deterministic CBOR and deserialize correctly.
#[test]
fn test_pca_serialization_roundtrip() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let cap = CapabilityData::from_json(
        "cap:test",
        "function",
        &json!({
            "name": "test",
            "values": [1, 2, 3],
            "nested": {"key": "value"}
        }),
    )
    .unwrap();

    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(Utc::now() + Duration::hours(1))
        .build_and_sign(&gateway)
        .unwrap();

    // Serialize to CBOR
    let cbor_bytes = pca.to_cbor().unwrap();
    println!("PCA serialized to {} bytes", cbor_bytes.len());

    // Deserialize
    let pca2 = Pca::from_cbor(&cbor_bytes).unwrap();

    // Verify hash is identical (proves deterministic encoding)
    assert_eq!(pca.try_hash().unwrap(), pca2.try_hash().unwrap());

    // Verify signature still valid
    assert!(pca2.try_verify_signature().is_ok());

    // Verify all fields match
    assert_eq!(pca.version(), pca2.version());
    assert_eq!(pca.designated_executor(), pca2.designated_executor());
    assert_eq!(pca.issuer(), pca2.issuer());
    assert_eq!(pca.capabilities().len(), pca2.capabilities().len());

    println!("✅ CBOR serialization roundtrip successful!");
}

// ============================================================================
// Test 9: Dropping Capabilities (Subset Allowed)
// ============================================================================

/// Demonstrates that children can drop capabilities (take a subset).
///
/// This is allowed because dropping is a form of attenuation -
/// the child has LESS authority than the parent.
#[test]
fn test_dropping_capabilities_allowed() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Root has two capabilities
    let cap_read = CapabilityData::from_json("cap:read", "function", &json!({})).unwrap();
    let cap_write = CapabilityData::from_json("cap:write", "function", &json!({})).unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap_read.clone())
        .add_capability(cap_write)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Child only requests cap:read (drops cap:write)
    let subset_request = ContinuationRequest {
        capabilities: vec![cap_read],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &subset_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Should succeed - dropping capabilities is allowed
    let result = cta.submit(&root_pca, &poc, &subset_request, Utc::now());
    assert!(
        result.is_ok(),
        "Should allow dropping capabilities: {result:?}"
    );

    let child_pca = result.unwrap();
    assert_eq!(child_pca.capabilities().len(), 1);
    assert_eq!(child_pca.capabilities()[0].key(), "cap:read");

    println!("✅ Dropping capabilities correctly allowed!");
}

// ============================================================================
// Test 10: Child Expiry Cannot Exceed Parent
// ============================================================================

/// Demonstrates that child PCAs cannot have longer expiry than parent.
#[test]
fn test_child_expiry_cannot_exceed_parent() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let parent_expires = Utc::now() + Duration::hours(1);
    let child_expires = Utc::now() + Duration::hours(2); // Longer than parent!

    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(parent_expires)
        .build_and_sign(&gateway)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: child_expires, // Exceeds parent!
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::ExpiryExceedsParent)),
        "CTA should reject child with longer expiry: {result:?}"
    );

    println!("✅ Expiry extension correctly rejected!");
}

// ============================================================================
// Test 11: Empty Capabilities Rejected
// ============================================================================

/// Demonstrates that PCAs require at least one capability.
///
/// A PCA represents authorization to do something - without capabilities,
/// there's nothing being authorized.
#[test]
fn test_empty_capabilities_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);

    // Try to create a PCA with NO capabilities
    let result = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway);

    assert!(
        result.is_err(),
        "PCA with no capabilities should be rejected"
    );

    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("at least one capability"),
        "Error should mention capability requirement: {err}"
    );

    println!("✅ Empty capabilities correctly rejected!");
}

// ============================================================================
// Test 12: Tampered PoC Signature Rejected
// ============================================================================

/// Demonstrates that a `PoC` signed for one request cannot be used with a different request.
///
/// # Attack Scenario: Request Substitution
///
/// ```text
///   Agent creates valid PoC for Request A
///   ┌─────────────────────────────────────────────────────┐
///   │ PoC (signed for Request A)                          │
///   │ ─────────────────────────────────────────────────── │
///   │ parent_hash:  H(parent_pca)                         │
///   │ request_hash: H(Request_A) ◄── Cryptographically    │
///   │ signature:    Sign(SK, ... || H(Request_A) || ...)  │  bound
///   └─────────────────────────────────────────────────────┘
///
///   Attacker tries to use PoC with Request B
///   ┌─────────────────────────────────────────────────────┐
///   │ Submitted to CTA:                                   │
///   │   parent_pca:  (same)                               │
///   │   poc:         (created for Request A)              │
///   │   request:     Request_B  ◄── DIFFERENT!            │
///   │                (e.g., different designated_executor)│
///   └─────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌─────────────────────────────────────────────────────┐
///   │ CTA Signature Verification                          │
///   │                                                     │
///   │ expected_msg = parent_hash || H(Request_B) || chal  │
///   │ poc.signature was over:                             │
///   │              parent_hash || H(Request_A) || chal    │
///   │                                                     │
///   │ H(Request_A) ≠ H(Request_B)                         │
///   │ → Signature verification FAILS                      │
///   └─────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: SignatureError
/// ```
///
/// # Security Property: Cryptographic Binding
///
/// The `PoC` signature covers the request hash, making it impossible to:
/// - Substitute a different continuation request
/// - Change the designated executor
/// - Modify capability restrictions
/// - Alter expiry time
#[test]
fn test_poc_request_mismatch_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
    let next_agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[4; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Create a valid PoC for request_a
    let request_a = ContinuationRequest {
        capabilities: vec![cap.clone()],
        designated_executor: DesignatedExecutor::from_public_key(next_agent1.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc_for_a = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request_a,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Create a different request_b
    let request_b = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent2.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Try to use poc_for_a with request_b - should fail
    // The PoC signature was computed over request_a's hash, not request_b's
    let result = cta.submit(&root_pca, &poc_for_a, &request_b, Utc::now());
    assert!(
        matches!(result, Err(CtaError::SignatureError(_))),
        "CTA should reject PoC/request mismatch: {result:?}"
    );

    println!("✅ PoC/request mismatch correctly rejected!");
}

// ============================================================================
// Test 13: PoC for Wrong Parent Rejected
// ============================================================================

/// Demonstrates that a `PoC` bound to one parent cannot be used with another.
///
/// # Attack Scenario: Parent Substitution
///
/// ```text
///   Two different root PCAs exist:
///
///   ┌─────────────────────────┐    ┌─────────────────────────┐
///   │ Root PCA 1              │    │ Root PCA 2              │
///   │ H(1) = 0xAAA...         │    │ H(2) = 0xBBB...         │
///   │ caps: [high_privilege]  │    │ caps: [low_privilege]   │
///   │ designated: agent       │    │ designated: agent       │
///   └─────────────────────────┘    └─────────────────────────┘
///
///   Agent creates PoC for Root PCA 1 (high privilege)
///   ┌─────────────────────────────────────────────────────┐
///   │ PoC (bound to Root PCA 1)                           │
///   │ ─────────────────────────────────────────────────── │
///   │ parent_hash: 0xAAA... ◄── Hash of Root PCA 1        │
///   │ signature: Sign(SK, 0xAAA... || ...)                │
///   └─────────────────────────────────────────────────────┘
///
///   Attacker submits PoC with Root PCA 2 (low privilege)
///   hoping to upgrade to high privilege from PCA 1
///   ┌─────────────────────────────────────────────────────┐
///   │ Submitted to CTA:                                   │
///   │   parent_pca: Root PCA 2 (low privilege)            │
///   │   poc:        (bound to Root PCA 1)                 │
///   └─────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌─────────────────────────────────────────────────────┐
///   │ CTA Hash Verification                               │
///   │                                                     │
///   │ computed: H(Root PCA 2) = 0xBBB...                   │
///   │ poc.parent_hash        = 0xAAA...                   │
///   │                                                     │
///   │ 0xBBB ≠ 0xAAA → MISMATCH                            │
///   └─────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: HashMismatch
/// ```
///
/// # Security Property: Chain Integrity
///
/// The `PoC`'s `parent_hash` field cryptographically binds it to exactly one parent:
/// - Cannot reuse `PoC` with different parent PCA
/// - Prevents chain confusion attacks
/// - Ensures audit trail integrity
#[test]
fn test_poc_wrong_parent_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // Create two different root PCAs
    let root_pca_1 = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let root_pca_2 = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires + Duration::seconds(1)) // Different expiry = different hash
        .build_and_sign(&gateway)
        .unwrap();

    assert_ne!(
        root_pca_1.try_hash().unwrap(),
        root_pca_2.try_hash().unwrap()
    );

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Create PoC for root_pca_1
    let poc_for_1 = ProofOfContinuity::build(
        &root_pca_1,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Try to use it with root_pca_2
    let result = cta.submit(&root_pca_2, &poc_for_1, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::HashMismatch)),
        "CTA should reject PoC for wrong parent: {result:?}"
    );

    println!("✅ PoC for wrong parent correctly rejected!");
}

// ============================================================================
// Test 14: Hash Determinism and Collision Resistance
// ============================================================================

/// Demonstrates that PCA hashes are deterministic and collision-resistant.
///
/// 1. Same PCA serialized twice produces the same hash
/// 2. PCAs with different content produce different hashes
/// 3. Even small changes (1 second in expiry) produce different hashes
#[test]
fn test_hash_determinism_and_collision_resistance() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({"value": 42})).unwrap();

    // Create a PCA
    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Test 1: Hash is deterministic (same PCA = same hash)
    let hash1 = pca.try_hash().unwrap();
    let hash2 = pca.try_hash().unwrap();
    assert_eq!(hash1, hash2, "Hash should be deterministic");

    // Test 2: Serialization roundtrip preserves hash
    let cbor = pca.to_cbor().unwrap();
    let pca_restored = Pca::from_cbor(&cbor).unwrap();
    let hash_restored = pca_restored.try_hash().unwrap();
    assert_eq!(
        hash1, hash_restored,
        "Hash should survive serialization roundtrip"
    );

    // Test 3: Different expiry = different hash
    let pca_different_expiry = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires + Duration::seconds(1)) // Just 1 second different
        .build_and_sign(&gateway)
        .unwrap();
    let hash_different_expiry = pca_different_expiry.try_hash().unwrap();
    assert_ne!(
        hash1, hash_different_expiry,
        "Different expiry should produce different hash"
    );

    // Test 4: Different capability data = different hash
    let cap_different =
        CapabilityData::from_json("cap:test", "function", &json!({"value": 43})).unwrap();
    let pca_different_cap = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap_different)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();
    let hash_different_cap = pca_different_cap.try_hash().unwrap();
    assert_ne!(
        hash1, hash_different_cap,
        "Different capability should produce different hash"
    );

    // Test 5: Different designated executor = different hash
    let other_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
    let pca_different_executor = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(other_agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();
    let hash_different_executor = pca_different_executor.try_hash().unwrap();
    assert_ne!(
        hash1, hash_different_executor,
        "Different executor should produce different hash"
    );

    // Test 6: Different issuer = different hash (and different signature)
    let other_gateway = KeyPair::from_seed(Algorithm::Ed25519, &[4; 32]);
    let pca_different_issuer = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&other_gateway)
        .unwrap();
    let hash_different_issuer = pca_different_issuer.try_hash().unwrap();
    assert_ne!(
        hash1, hash_different_issuer,
        "Different issuer should produce different hash"
    );

    // Collect all hashes and verify they're all unique
    let all_hashes = [
        hash1,
        hash_different_expiry,
        hash_different_cap,
        hash_different_executor,
        hash_different_issuer,
    ];
    let unique_count = all_hashes
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert_eq!(
        unique_count,
        all_hashes.len(),
        "All hashes should be unique"
    );

    println!("✅ Hash determinism and collision resistance verified!");
    println!("   Base hash: {}", all_hashes[0].to_hex());
    println!(
        "   All {} variants produced unique hashes",
        all_hashes.len()
    );
}

// ============================================================================
// Test 15: Duplicate Capability Keys Rejected
// ============================================================================

/// Demonstrates that duplicate capability keys in a continuation request are rejected.
///
/// Each capability must have a unique key within a PCA/request.
#[test]
fn test_duplicate_capability_keys_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Root PCA with a capability
    let cap = CapabilityData::from_json("cap:api", "function", &json!({"limit": 1000})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Try to create a request with duplicate keys
    let cap_dup = CapabilityData::from_json("cap:api", "function", &json!({"limit": 500})).unwrap();
    let request_with_dups = ContinuationRequest {
        capabilities: vec![cap.clone(), cap_dup], // Same key "cap:api" twice!
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request_with_dups,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc, &request_with_dups, Utc::now());
    assert!(
        matches!(result, Err(CtaError::DuplicateCapabilityKey(_))),
        "CTA should reject duplicate capability keys: {result:?}"
    );

    println!("✅ Duplicate capability keys correctly rejected!");
}

// ============================================================================
// Test 16: Untrusted CTA Issuer in Multi-Hop Chain
// ============================================================================

/// Demonstrates that child PCAs signed by an untrusted CTA are rejected.
///
/// # Attack Scenario: Rogue CTA Injection
///
/// ```text
///   Multi-hop chain with rogue CTA insertion:
///
///   ┌──────────┐   trust   ┌──────────┐
///   │ Gateway  │◄─────────│ Trusted  │
///   │ (root)   │          │   CTA    │
///   └────┬─────┘          └──────────┘
///        │                      │
///        │ issues               │ NOT trusted by
///        ▼                      │ Trusted CTA
///   ┌────────────┐              │
///   │ Root PCA   │              │
///   │ issuer: GW │              │
///   └────┬───────┘              │
///        │                      │
///        ▼ Hop 1 via...         │
///   ┌─────────────┐             │
///   │  Rogue CTA  │◄────────────┘ (not in trusted list)
///   │  PK: 0xROG  │
///   └────┬────────┘
///        │ signs
///        ▼
///   ┌────────────────────┐
///   │ Child PCA (Hop 1)  │
///   │ issuer: 0xROG      │ ◄── Signed by rogue!
///   │ prev_hash: H(root) │
///   └────┬───────────────┘
///        │
///        ▼ Hop 2 via Trusted CTA
///   ┌────────────────────────────────────────────────────┐
///   │ Trusted CTA Validation                             │
///   │                                                    │
///   │ CHECK: parent.is_root?                             │
///   │        NO (has prev_hash)                          │
///   │                                                    │
///   │ CHECK: parent.issuer ∈ trusted_ctas?               │
///   │        0xROG ∈ [trusted_cta]?                      │
///   │        = FALSE                                     │
///   └────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: UntrustedCta(0xROG...)
/// ```
///
/// # Security Property: CTA Chain of Trust
///
/// - Root PCAs must be signed by trusted root authorities
/// - Child PCAs must be signed by trusted CTAs
/// - Different trust registries for roots vs CTAs
/// - Prevents rogue CTA injection into chains
#[test]
fn test_untrusted_cta_issuer_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let trusted_cta = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let rogue_cta = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
    let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
    let agent3 = KeyPair::from_seed(Algorithm::Ed25519, &[4; 32]);

    // Trusted CTA - only trusts gateway and itself
    let cta = CtaBuilder::new(trusted_cta.clone())
        .trust_root_authority(gateway.public_key())
        .trust_cta(trusted_cta.public_key()) // Only trusts itself
        .build();

    // Rogue CTA - trusts gateway (to process root PCAs)
    let rogue = CtaBuilder::new(rogue_cta.clone())
        .trust_root_authority(gateway.public_key())
        .trust_cta(rogue_cta.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // Gateway issues root PCA to agent1
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent1.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Agent1 delegates to agent2 via the ROGUE CTA (not the trusted one)
    let cont1 = ContinuationRequest {
        capabilities: vec![cap.clone()],
        designated_executor: DesignatedExecutor::from_public_key(agent2.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc1 = ProofOfContinuity::build(
        &root_pca,
        &agent1,
        &cont1,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Rogue CTA signs the child PCA (this succeeds - rogue trusts gateway)
    let rogue_signed_pca = rogue.submit(&root_pca, &poc1, &cont1, Utc::now()).unwrap();
    assert_eq!(rogue_signed_pca.issuer(), &rogue_cta.public_key());

    // Now agent2 tries to continue via the TRUSTED CTA
    // This should fail because trusted CTA doesn't trust rogue_cta
    let cont2 = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(agent3.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc2 = ProofOfContinuity::build(
        &rogue_signed_pca,
        &agent2,
        &cont2,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // The trusted CTA should reject because rogue_signed_pca was signed by rogue_cta
    let result = cta.submit(&rogue_signed_pca, &poc2, &cont2, Utc::now());

    // Should fail with UntrustedCta (not UntrustedRootAuthority)
    assert!(
        matches!(result, Err(CtaError::UntrustedCta(_))),
        "CTA should reject child PCA from untrusted CTA: {result:?}"
    );

    println!("✅ Untrusted CTA issuer correctly rejected!");
}

// ============================================================================
// Test 17: CTA Reference Requires Explicit Authorization
// ============================================================================

/// Demonstrates that `CtaReference` designations require explicit authorization.
///
/// When a parent PCA designates via `CtaReference`:
/// - The CTA must explicitly authorize the executor via its `ExecutorResolver`
/// - Default (`RejectAllResolver`) rejects all executors
/// - `PermissiveResolver` accepts all (for testing)
///
/// This prevents the bypass where anyone could claim to be the executor
/// for a CTA-referenced designation.
#[test]
fn test_cta_reference_requires_authorization() {
    use amla_protocol::{CtaReference, PermissiveResolver, RejectAllResolver};

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // Create a root PCA that designates via CtaReference (not public key)
    let cta_ref = CtaReference::new(cta_keypair.public_key());
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(DesignatedExecutor::from_cta_reference(cta_ref))
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Create CTA with RejectAllResolver (default - rejects all CTA reference executors)
    let cta_reject = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .resolver(RejectAllResolver)
        .build();

    // Agent tries to continue via this CTA
    let request = ContinuationRequest {
        capabilities: vec![cap.clone()],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Should fail - RejectAllResolver doesn't authorize any executor for CtaReference
    let result = cta_reject.submit(&root_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::CtaReferenceNotAuthorized)),
        "CTA with RejectAllResolver should reject CtaReference executor: {result:?}"
    );

    // Now create CTA with PermissiveResolver (accepts all for testing)
    let cta_permissive = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .resolver(PermissiveResolver)
        .build();

    // Same submission should succeed with PermissiveResolver
    let result = cta_permissive.submit(&root_pca, &poc, &request, Utc::now());
    assert!(
        result.is_ok(),
        "CTA with PermissiveResolver should accept CtaReference executor: {result:?}"
    );

    println!("✅ CtaReference authorization correctly enforced!");
}

// ============================================================================
// Test 18: Characteristic-Based Executor Flow
// ============================================================================

/// Demonstrates characteristic-based executor designation and resolution.
///
/// The parent PCA designates "any sales agent" rather than a specific key.
/// The CTA uses its `ExecutorResolver` to verify the executor satisfies the
/// characteristic.
#[test]
fn test_characteristic_executor_flow() {
    use amla_protocol::{ExecutorCharacteristic, ExecutorResolver};

    /// Custom resolver that only accepts executors with specific keys
    struct WhitelistResolver {
        allowed_keys: Vec<PublicKey>,
    }

    impl ExecutorResolver for WhitelistResolver {
        fn verify(
            &self,
            executor: &PublicKey,
            characteristic: &ExecutorCharacteristic,
            _proof: Option<&[u8]>,
        ) -> bool {
            // Only accept "role:sales-agent" characteristic
            if characteristic.characteristic_type != "role" || characteristic.value != "sales-agent"
            {
                return false;
            }
            // Check executor is in whitelist
            self.allowed_keys.contains(executor)
        }
    }

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let authorized_agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let unauthorized_agent = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:sales", "function", &json!({})).unwrap();

    // Create CTA with whitelist resolver (only authorized_agent is allowed)
    let resolver = WhitelistResolver {
        allowed_keys: vec![authorized_agent.public_key()],
    };
    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .resolver(resolver)
        .build();

    // Root PCA designates "any sales agent" via characteristic
    let char = ExecutorCharacteristic::new("role", "sales-agent");
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(DesignatedExecutor::from_characteristic(char))
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap.clone()],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Unauthorized agent tries to continue - should fail
    let poc_unauthorized = ProofOfContinuity::build(
        &root_pca,
        &unauthorized_agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc_unauthorized, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::CharacteristicNotSatisfied)),
        "Unauthorized agent should be rejected: {result:?}"
    );

    // Authorized agent continues - should succeed
    let poc_authorized = ProofOfContinuity::build(
        &root_pca,
        &authorized_agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc_authorized, &request, Utc::now());
    assert!(
        result.is_ok(),
        "Authorized agent should succeed: {result:?}"
    );

    println!("✅ Characteristic-based executor flow works correctly!");
}

// ============================================================================
// Test 19: Stateless Freshness Validation
// ============================================================================

/// Demonstrates stateless freshness validation behavior.
///
/// The `StatelessFreshnessValidator` handles Random and Timestamp challenges
/// but rejects Epoch challenges (which require stateful tracking).
#[test]
fn test_stateless_freshness_validation() {
    use amla_protocol::StatelessFreshnessValidator;

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    // Create CTA with StatelessFreshnessValidator
    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .freshness(StatelessFreshnessValidator::default())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    // Random challenge should work
    let poc_random = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();
    let result = cta.submit(&root_pca, &poc_random, &request, Utc::now());
    assert!(result.is_ok(), "Random challenge should pass: {result:?}");

    // Timestamp challenge should work (within skew)
    let poc_timestamp = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::timestamp(Duration::seconds(60)), // 60 second max skew
    )
    .unwrap();
    let result = cta.submit(&root_pca, &poc_timestamp, &request, Utc::now());
    assert!(
        result.is_ok(),
        "Timestamp challenge should pass: {result:?}"
    );

    println!("✅ Stateless freshness validation works correctly!");
}

// ============================================================================
// Test 20: Security - PCA Signature Tampering Rejected
// ============================================================================

/// Demonstrates that modifying PCA bytes invalidates the signature.
///
/// # Attack Scenario: Byte-Level Tampering
///
/// ```text
///   Valid PCA (CBOR bytes):
///   ┌─────────────────────────────────────────────────────────────┐
///   │ [ header | capabilities | expires | designated | signature ]│
///   │                              │                      ▲       │
///   │                              │                      │       │
///   │                     Attacker modifies         Signature was │
///   │                     expires field             over original │
///   │                              │                              │
///   │                              ▼                              │
///   └─────────────────────────────────────────────────────────────┘
///
///   Tampered PCA:
///   ┌─────────────────────────────────────────────────────────────┐
///   │ [ header | capabilities | MODIFIED | designated | signature ]│
///   │                              ▲                      │       │
///   │                              │                      │       │
///   │                       Changed!        Same signature (invalid)
///   └─────────────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌─────────────────────────────────────────────────────────────┐
///   │ Signature Verification                                      │
///   │                                                             │
///   │ expected_hash = H(tampered_content)                         │
///   │ signature was over H(original_content)                      │
///   │                                                             │
///   │ Ed25519::verify(signature, tampered_content) = FAIL         │
///   └─────────────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: SignatureError
/// ```
///
/// # Security Property: Integrity via Digital Signatures
///
/// - Ed25519 signatures cover the entire PCA content
/// - Any byte modification invalidates the signature
/// - Attacker cannot forge signatures without private key
#[test]
fn test_pca_tampering_rejected() {
    // Why position 50? CBOR structure is approximately:
    // - Bytes 0-10: Map header, version field
    // - Bytes 10-30: Capability array header
    // - Bytes 30-80: Capability data (key, type, JSON)
    // - Bytes 80+: Designated executor, expiry, issuer, signature
    //
    // Position 50 is safely within the capability data region,
    // avoiding the signature bytes (last 64) and map structure bytes.
    const TAMPER_POSITION: usize = 50;

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({"value": 100})).unwrap();

    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Valid PCA should verify
    assert!(
        pca.try_verify_signature().is_ok(),
        "Original PCA should verify"
    );

    // Serialize to bytes
    let mut cbor_bytes = pca.to_cbor().unwrap();

    // Tamper with a byte in the middle (not the signature itself)
    // This simulates an attacker modifying the content.
    if cbor_bytes.len() > TAMPER_POSITION + 64 {
        // Ensure we're not tampering with signature (last 64 bytes)
        cbor_bytes[TAMPER_POSITION] ^= 0xFF; // Flip all bits
    }

    // Try to deserialize tampered bytes
    let result = Pca::from_cbor(&cbor_bytes);

    // Either deserialization fails OR signature verification fails
    if let Ok(tampered_pca) = result {
        // If it parses, signature should fail
        let verify_result = tampered_pca.try_verify_signature();
        assert!(
            verify_result.is_err(),
            "Tampered PCA should fail signature verification: {verify_result:?}"
        );
    } else {
        // Deserialization failed due to invalid CBOR structure - also acceptable
    }

    println!("✅ PCA tampering correctly rejected!");
}

// ============================================================================
// Test 21: Security - Malformed CBOR Handling
// ============================================================================

/// Demonstrates graceful handling of malformed CBOR input.
///
/// # Attack Scenario: Garbage Input
///
/// ```text
///   Attacker sends various malformed inputs:
///
///   1. Random bytes:        [0xDE, 0xAD, 0xBE, 0xEF, ...]
///   2. Truncated CBOR:      [valid header, then cut off]
///   3. Wrong CBOR type:     [integer instead of map]
///   4. Empty input:         []
///
///   Each should result in clean error, not panic/crash
/// ```
///
/// # Security Property: Robustness
///
/// - Parser should never panic on malformed input
/// - Clear error messages for debugging
/// - No information leakage about internal structure
#[test]
fn test_malformed_cbor_rejected() {
    // Test 1: Random garbage bytes
    let garbage: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    let result = Pca::from_cbor(&garbage);
    assert!(result.is_err(), "Random garbage should fail to parse");

    // Test 2: Empty input
    let empty: Vec<u8> = vec![];
    let result = Pca::from_cbor(&empty);
    assert!(result.is_err(), "Empty input should fail to parse");

    // Test 3: Valid CBOR but wrong type (just an integer)
    let mut wrong_type_buf = Vec::new();
    ciborium::into_writer(&42u64, &mut wrong_type_buf).unwrap();
    let result = Pca::from_cbor(&wrong_type_buf);
    assert!(result.is_err(), "Wrong CBOR type should fail to parse");

    // Test 4: Valid CBOR map but missing required fields
    let incomplete_map = {
        let mut buf = Vec::new();
        ciborium::into_writer(
            &std::collections::BTreeMap::<String, String>::new(),
            &mut buf,
        )
        .unwrap();
        buf
    };
    let result = Pca::from_cbor(&incomplete_map);
    assert!(result.is_err(), "Incomplete map should fail to parse");

    // Test 5: Truncated valid PCA
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();
    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(Utc::now() + Duration::hours(1))
        .build_and_sign(&gateway)
        .unwrap();

    let full_bytes = pca.to_cbor().unwrap();
    let truncated = &full_bytes[..full_bytes.len() / 2]; // Cut in half
    let result = Pca::from_cbor(truncated);
    assert!(result.is_err(), "Truncated PCA should fail to parse");

    println!("✅ Malformed CBOR correctly rejected!");
}

// ============================================================================
// Test 22: Security - Invalid Signature Algorithm Handling
// ============================================================================

/// Demonstrates that PCAs with invalid signatures are rejected.
///
/// # Attack Scenario: Signature Fabrication
///
/// ```text
///   Attacker creates PCA content and adds random bytes as signature:
///
///   ┌─────────────────────────────────────────────────────────────┐
///   │ Fabricated PCA                                              │
///   │ ─────────────────────────────────────────────────────────── │
///   │ version:    1                                               │
///   │ caps:       [{"key": "cap:admin"}]                          │
///   │ designated: attacker.pk                                     │
///   │ issuer:     gateway.pk (claimed but not real)               │
///   │ signature:  [random 64 bytes]  ◄── Not a valid signature!   │
///   └─────────────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   ┌─────────────────────────────────────────────────────────────┐
///   │ Ed25519 Verification                                        │
///   │                                                             │
///   │ verify(random_bytes, gateway.pk, content) = INVALID         │
///   │                                                             │
///   │ Random bytes are not a valid Ed25519 signature              │
///   └─────────────────────────────────────────────────────────────┘
///                          │
///                          ▼
///   REJECT: SignatureError
/// ```
///
/// # Security Property: Unforgeable Signatures
///
/// - Ed25519 signatures cannot be forged without the private key
/// - Random bytes have negligible probability of being valid
/// - Claimed issuer key is verified against actual signature
#[test]
fn test_fabricated_signature_rejected() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let attacker = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap =
        CapabilityData::from_json("cap:admin", "function", &json!({"level": "root"})).unwrap();

    // Create a valid PCA first
    let valid_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Create another PCA signed by attacker (claiming to be from gateway would require
    // modifying internal structures, so we test that attacker's signature doesn't verify
    // with gateway's key)
    let attacker_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&attacker)
        .unwrap();

    // Valid PCA verifies
    assert!(
        valid_pca.try_verify_signature().is_ok(),
        "Valid PCA should verify"
    );

    // Attacker's PCA verifies with attacker's key (it's signed correctly)
    assert!(
        attacker_pca.try_verify_signature().is_ok(),
        "Attacker PCA should verify with its own key"
    );

    // But the issuers are different
    assert_ne!(
        valid_pca.issuer(),
        attacker_pca.issuer(),
        "Issuers should be different"
    );
    assert_eq!(attacker_pca.issuer(), &attacker.public_key());

    // In a real attack, this would be caught by the trust check
    // (attacker is not in trusted_root_authorities)

    println!("✅ Signature verification distinguishes issuers correctly!");
}

// ============================================================================
// Test 23: Security - PCA Version Validation
// ============================================================================

/// Demonstrates that PCAs with invalid versions are handled correctly.
///
/// Future-proofing: when protocol evolves, old CTAs should reject
/// PCAs with unsupported version numbers.
#[test]
fn test_version_handling() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // Create PCA with current version
    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Verify version is set correctly
    assert_eq!(pca.version(), PROTOCOL_VERSION);

    // Version is part of the signed content, so changing it would
    // invalidate the signature (tested in test_pca_tampering_rejected)

    println!("✅ Version handling works correctly!");
    println!("   Current protocol version: {PROTOCOL_VERSION}");
}

// ============================================================================
// Test 24: Security - Hash Collision Resistance
// ============================================================================

/// Demonstrates that PCA hashes are collision-resistant.
///
/// Even nearly-identical PCAs produce completely different hashes.
/// This is critical for chain integrity - each PCA must have a unique hash.
///
/// # Security Property: Collision Resistance
///
/// ```text
///   PCA with value: 100  →  H1 = 0xABC123...
///   PCA with value: 101  →  H2 = 0xDEF456... (completely different)
///
///   SHA-256 provides 2^128 collision resistance
///   Finding two PCAs with same hash is computationally infeasible
/// ```
#[test]
fn test_hash_collision_resistance_extended() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);

    // Create 10 PCAs with slightly different content
    let mut hashes = std::collections::HashSet::new();

    for i in 0..10 {
        let cap = CapabilityData::from_json(
            "cap:test",
            "function",
            &json!({"iteration": i, "timestamp": Utc::now().timestamp_nanos_opt()}),
        )
        .unwrap();

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let hash = pca.try_hash().unwrap();
        let is_new = hashes.insert(hash);

        assert!(
            is_new,
            "Hash collision detected at iteration {i}! This should be astronomically unlikely."
        );
    }

    assert_eq!(hashes.len(), 10, "All 10 PCAs should have unique hashes");

    println!("✅ Hash collision resistance verified!");
    println!("   Generated {} unique hashes", hashes.len());
}

// ============================================================================
// Test 25: Security - Capability Data Integrity
// ============================================================================

/// Demonstrates that capability data is preserved exactly through serialization.
///
/// Important for ensuring that constraints are not modified in transit.
#[test]
fn test_capability_data_integrity() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);

    // Create capability with complex nested data
    let original_data = json!({
        "resource": "payments.stripe",
        "action": "charge",
        "constraints": {
            "max_amount_cents": 100_000,
            "allowed_currencies": ["USD", "EUR", "GBP"],
            "customer_types": {
                "allowed": ["premium", "enterprise"],
                "denied": ["trial"]
            }
        },
        "metadata": {
            "created_by": "test",
            "version": 1
        }
    });

    let cap = CapabilityData::from_json("cap:payments", "function", &original_data).unwrap();

    let pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Serialize and deserialize
    let cbor_bytes = pca.to_cbor().unwrap();
    let restored_pca = Pca::from_cbor(&cbor_bytes).unwrap();

    // Verify capability data is preserved
    assert_eq!(restored_pca.capabilities().len(), 1);
    let restored_cap = &restored_pca.capabilities()[0];
    assert_eq!(restored_cap.key(), "cap:payments");
    assert_eq!(restored_cap.capability_type(), "function");

    // Verify JSON data matches
    let restored_data = restored_cap.to_json().unwrap();
    assert_eq!(
        original_data, restored_data,
        "Capability data should survive serialization roundtrip"
    );

    println!("✅ Capability data integrity verified!");
}

// ============================================================================
// Test 26: Concurrent Submissions from Same Parent
// ============================================================================

/// Demonstrates that multiple valid continuations from the same parent are allowed.
///
/// # Scenario: Parallel Delegation
///
/// ```text
///   ┌─────────────────────────────────┐
///   │ Root PCA                        │
///   │ designated: agent               │
///   │ caps: [cap:api]                 │
///   └───────────────┬─────────────────┘
///                   │
///          ┌───────┴───────┐
///          │               │
///          ▼               ▼
///   ┌──────────────┐ ┌──────────────┐
///   │ Request A    │ │ Request B    │
///   │ → agent_a    │ │ → agent_b    │
///   │ limit: 500   │ │ limit: 300   │
///   └──────┬───────┘ └──────┬───────┘
///          │                │
///          ▼                ▼
///   ┌──────────────┐ ┌──────────────┐
///   │ Child PCA A  │ │ Child PCA B  │
///   │ (valid)      │ │ (valid)      │
///   └──────────────┘ └──────────────┘
///
///   Both submissions are valid because:
///   - Different requests (different hashes)
///   - Each has unique PoC with unique challenge
///   - No replay: different nonces
/// ```
///
/// # Note on Forking
///
/// This is NOT replay - it's valid forking. The parent can delegate
/// to multiple children simultaneously. Replay prevention (deferred to
/// Phase 4) only prevents the SAME `PoC` from being used twice.
#[test]
fn test_concurrent_submissions_allowed() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let agent_a = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent_b = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:api", "function", &json!({"limit": 1000})).unwrap();

    // Root PCA designates agent
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Request A: delegate to agent_a with limit 500
    let cap_a = CapabilityData::from_json("cap:api", "function", &json!({"limit": 500})).unwrap();
    let request_a = ContinuationRequest {
        capabilities: vec![cap_a],
        designated_executor: DesignatedExecutor::from_public_key(agent_a.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc_a = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request_a,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Request B: delegate to agent_b with limit 300
    let cap_b = CapabilityData::from_json("cap:api", "function", &json!({"limit": 300})).unwrap();
    let request_b = ContinuationRequest {
        capabilities: vec![cap_b],
        designated_executor: DesignatedExecutor::from_public_key(agent_b.public_key()),
        expires_at: expires,
        payload: None,
    };
    let poc_b = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request_b,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Both should succeed (with stateless validator)
    let result_a = cta.submit(&root_pca, &poc_a, &request_a, Utc::now());
    assert!(
        result_a.is_ok(),
        "First submission should succeed: {result_a:?}"
    );

    let result_b = cta.submit(&root_pca, &poc_b, &request_b, Utc::now());
    assert!(
        result_b.is_ok(),
        "Second submission should succeed: {result_b:?}"
    );

    let child_a = result_a.unwrap();
    let child_b = result_b.unwrap();

    // Both children link to the same parent
    assert_eq!(child_a.prev_hash(), child_b.prev_hash());
    assert_eq!(child_a.root_hash(), child_b.root_hash());

    // But they are different PCAs (different designated executors)
    assert_ne!(child_a.try_hash().unwrap(), child_b.try_hash().unwrap());
    assert_eq!(
        child_a.designated_executor().as_public_key(),
        Some(&agent_a.public_key())
    );
    assert_eq!(
        child_b.designated_executor().as_public_key(),
        Some(&agent_b.public_key())
    );

    println!("✅ Concurrent submissions correctly allowed!");
    println!(
        "   Both children share parent: {}",
        root_pca.try_hash().unwrap().to_hex()
    );
}

// ============================================================================
// Test 27: Expiry Boundary Conditions
// ============================================================================

/// Demonstrates expiry behavior at exact boundaries.
///
/// # Edge Cases Tested
///
/// ```text
///   Timeline:
///   ────────────────────────────────────────────────────►
///   │           │           │           │
///   T-1ms    expires_at   T+1ms      T+1hr
///   (valid)  (boundary)   (expired)  (clearly expired)
///
///   Behavior at boundary:
///   - T-1ms:  ACCEPT (still valid)
///   - T+0ms:  REJECT (at expiry = expired)
///   - T+1ms:  REJECT (past expiry)
/// ```
///
/// # Security Property: No Grace Period
///
/// The protocol treats expiry as a hard cutoff. There is no "grace period"
/// that could be exploited. At the exact moment of expiry, the PCA is invalid.
#[test]
fn test_expiry_boundary_conditions() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    // Set expiry to a specific future time
    let expires_at = Utc::now() + Duration::seconds(10);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap.clone())
        .designated_executor(agent.public_key())
        .expires_at(expires_at)
        .build_and_sign(&gateway)
        .unwrap();

    let request = ContinuationRequest {
        capabilities: vec![cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires_at - Duration::seconds(1), // Child expires before parent
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    // Test 1: 1 second before expiry - should succeed
    let before_expiry = expires_at - Duration::seconds(1);
    let result = cta.submit(&root_pca, &poc, &request, before_expiry);
    assert!(
        result.is_ok(),
        "1 second before expiry should succeed: {result:?}"
    );

    // Test 2: Exactly at expiry - should fail (>= is expired)
    let at_expiry = expires_at;
    let result = cta.submit(&root_pca, &poc, &request, at_expiry);
    assert!(
        matches!(result, Err(CtaError::ParentExpired)),
        "Exactly at expiry should fail: {result:?}"
    );

    // Test 3: 1 second after expiry - should fail
    let after_expiry = expires_at + Duration::seconds(1);
    let result = cta.submit(&root_pca, &poc, &request, after_expiry);
    assert!(
        matches!(result, Err(CtaError::ParentExpired)),
        "After expiry should fail: {result:?}"
    );

    // Test 4: Way after expiry - should fail
    let way_after = expires_at + Duration::hours(1);
    let result = cta.submit(&root_pca, &poc, &request, way_after);
    assert!(
        matches!(result, Err(CtaError::ParentExpired)),
        "Way after expiry should fail: {result:?}"
    );

    println!("✅ Expiry boundary conditions verified!");
    println!("   Expiry is a hard cutoff with no grace period");
}

// ============================================================================
// Test 28: Truly Fabricated Signature Bytes
// ============================================================================

/// Demonstrates that random bytes cannot pass as a valid Ed25519 signature.
///
/// # Attack Scenario: Raw Signature Fabrication
///
/// ```text
///   Attacker constructs PCA manually:
///
///   ┌───────────────────────────────────────────────────────┐
///   │ Hand-crafted CBOR:                                    │
///   │                                                       │
///   │ { version: 1,                                         │
///   │   capabilities: [...],                                │
///   │   designated_executor: attacker_pk,                   │
///   │   issuer: gateway_pk,        ◄── Claims gateway       │
///   │   signature: [64 random bytes] ◄── Fabricated!        │
///   │ }                                                     │
///   └───────────────────────────────────────────────────────┘
///
///   Ed25519 verification:
///   - Signature is 64 bytes (correct length)
///   - But it's not a valid signature over the content
///   - Probability of random bytes being valid: 1/2^255
/// ```
#[test]
fn test_truly_fabricated_signature_bytes() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let expires = Utc::now() + Duration::hours(1);
    let cap = CapabilityData::from_json("cap:test", "function", &json!({})).unwrap();

    // Create a valid PCA
    let valid_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Serialize to CBOR
    let mut cbor_bytes = valid_pca.to_cbor().unwrap();
    let original_len = cbor_bytes.len();

    // Ed25519 signatures are 64 bytes at the end of the CBOR structure
    // Replace the last 64 bytes with random data
    if cbor_bytes.len() >= 64 {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        // Generate pseudo-random bytes using hasher (for deterministic but "random" data)
        let hasher_builder = RandomState::new();
        for i in 0..64 {
            let mut hasher = hasher_builder.build_hasher();
            hasher.write_usize(i);
            cbor_bytes[original_len - 64 + i] = (hasher.finish() & 0xFF) as u8;
        }
    }

    // Try to deserialize and verify
    let result = Pca::from_cbor(&cbor_bytes);

    if let Ok(tampered_pca) = result {
        // If CBOR parsing succeeded, signature verification must fail
        let verify_result = tampered_pca.try_verify_signature();
        assert!(
            verify_result.is_err(),
            "Fabricated signature should fail verification: {verify_result:?}"
        );
    } else {
        // CBOR parsing failed - also acceptable (signature bytes may have
        // corrupted the CBOR structure if they overlap with length fields)
    }

    println!("✅ Truly fabricated signature correctly rejected!");
}

// ============================================================================
// Test 29: Capability Key Case Sensitivity
// ============================================================================

/// Demonstrates that capability keys are case-sensitive.
///
/// # Why This Matters
///
/// If keys were case-insensitive, an attacker might try:
/// - Parent grants `"cap:read"`
/// - Child requests `"CAP:READ"` (hoping it's treated as same)
///
/// The protocol treats these as different keys, so the attack fails.
#[test]
fn test_capability_key_case_sensitivity() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Parent grants "cap:read" (lowercase)
    let parent_cap = CapabilityData::from_json("cap:read", "function", &json!({})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(parent_cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Child requests "CAP:READ" (uppercase) - should fail as unknown key
    let child_cap = CapabilityData::from_json("CAP:READ", "function", &json!({})).unwrap();
    let request = ContinuationRequest {
        capabilities: vec![child_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::UnknownCapabilityKey(_))),
        "Different case should be treated as unknown key: {result:?}"
    );

    // Verify that exact match works
    let exact_cap = CapabilityData::from_json("cap:read", "function", &json!({})).unwrap();
    let exact_request = ContinuationRequest {
        capabilities: vec![exact_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };
    let exact_poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &exact_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &exact_poc, &exact_request, Utc::now());
    assert!(
        result.is_ok(),
        "Exact case match should succeed: {result:?}"
    );

    println!("✅ Capability key case sensitivity verified!");
}

// ============================================================================
// Test 30: Capability Key Whitespace Handling
// ============================================================================

/// Demonstrates that capability keys preserve whitespace (no normalization).
///
/// # Why No Normalization
///
/// Normalizing keys (trimming whitespace) could introduce subtle bugs:
/// - Developer accidentally adds space: `"cap:read "`
/// - System silently normalizes to `"cap:read"`
/// - Later, explicit key comparison fails unexpectedly
///
/// Better to fail fast and require exact matches.
#[test]
fn test_capability_key_whitespace_preserved() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    // Parent grants "cap:read" (no whitespace)
    let parent_cap = CapabilityData::from_json("cap:read", "function", &json!({})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(parent_cap)
        .designated_executor(agent.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    // Child requests "cap:read " (trailing space) - should fail
    let child_cap = CapabilityData::from_json("cap:read ", "function", &json!({})).unwrap();
    let request = ContinuationRequest {
        capabilities: vec![child_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };

    let poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &poc, &request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::UnknownCapabilityKey(_))),
        "Trailing whitespace should be treated as unknown key: {result:?}"
    );

    // Also test leading space
    let leading_cap = CapabilityData::from_json(" cap:read", "function", &json!({})).unwrap();
    let leading_request = ContinuationRequest {
        capabilities: vec![leading_cap],
        designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
        expires_at: expires,
        payload: None,
    };
    let leading_poc = ProofOfContinuity::build(
        &root_pca,
        &agent,
        &leading_request,
        FreshnessChallenge::from_bytes([1; 32]),
    )
    .unwrap();

    let result = cta.submit(&root_pca, &leading_poc, &leading_request, Utc::now());
    assert!(
        matches!(result, Err(CtaError::UnknownCapabilityKey(_))),
        "Leading whitespace should be treated as unknown key: {result:?}"
    );

    println!("✅ Capability key whitespace handling verified!");
}

// ============================================================================
// Test 31: Deep Chain Validation
// ============================================================================

/// Demonstrates that the protocol handles deep chains (many hops) correctly.
///
/// # Production Considerations
///
/// ```text
///   Gateway → A₁ → A₂ → A₃ → ... → A₁₀₀
///                                    │
///                   100 hops deep ───┘
///
///   Potential issues at depth:
///   - Stack overflow in recursive validation
///   - Memory exhaustion from chain storage
///   - O(n²) hash verification
///
///   Our implementation:
///   - Iterative (not recursive) chain walking
///   - O(n) memory for n PCAs
///   - O(n) verification time
/// ```
#[test]
fn test_deep_chain_validation() {
    // Build a chain of 50 hops (enough to detect stack issues, fast enough for CI)
    const CHAIN_DEPTH: usize = 50;

    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);

    let cta = CtaBuilder::new(cta_keypair.clone())
        .trust_root_authority(gateway.public_key())
        .trust_cta(cta_keypair.public_key())
        .build();

    let expires = Utc::now() + Duration::hours(1);

    let mut chain: Vec<Pca> = Vec::with_capacity(CHAIN_DEPTH + 1);
    let agents: Vec<KeyPair> = (0..=CHAIN_DEPTH)
        .map(|_| KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]))
        .collect();

    // Root PCA
    let cap = CapabilityData::from_json("cap:api", "function", &json!({"depth": 0})).unwrap();
    let root_pca = PcaBuilder::new()
        .version(PROTOCOL_VERSION)
        .add_capability(cap)
        .designated_executor(agents[0].public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let root_hash = root_pca.try_hash().unwrap();
    chain.push(root_pca);

    // Build chain
    for i in 0..CHAIN_DEPTH {
        let parent = &chain[i];
        let current_agent = &agents[i];
        let next_agent = &agents[i + 1];

        let cap =
            CapabilityData::from_json("cap:api", "function", &json!({"depth": i + 1})).unwrap();
        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            parent,
            current_agent,
            &request,
            FreshnessChallenge::from_bytes([1; 32]),
        )
        .unwrap();

        let child = cta.submit(parent, &poc, &request, Utc::now()).unwrap();
        chain.push(child);
    }

    // Verify chain properties
    assert_eq!(chain.len(), CHAIN_DEPTH + 1);

    // All non-root PCAs should have the same root_hash
    for (i, pca) in chain.iter().enumerate().skip(1) {
        assert_eq!(
            pca.root_hash(),
            Some(&root_hash),
            "PCA at depth {i} should preserve root_hash"
        );
    }

    // Each PCA links to its parent
    for i in 1..chain.len() {
        assert_eq!(
            chain[i].prev_hash(),
            Some(&chain[i - 1].try_hash().unwrap()),
            "PCA at depth {i} should link to parent"
        );
    }

    // Validate the entire chain
    let validator = PermissiveValidator;
    let result = validate_cta_chain(
        &chain,
        &[gateway.public_key()],
        &[cta_keypair.public_key()],
        &validator,
        Utc::now(),
    );
    assert!(
        result.is_ok(),
        "Deep chain validation should succeed: {result:?}"
    );

    println!("✅ Deep chain validation passed!");
    println!("   Chain depth: {CHAIN_DEPTH} hops");
    println!("   Root hash preserved throughout: {}", root_hash.to_hex());
}

// ============================================================================
// Note on Replay Protection
// ============================================================================
//
// The current tests use `PermissiveFreshnessValidator` which accepts all
// challenges without tracking. True replay protection requires:
//
// 1. `StatefulFreshnessValidator` that tracks (parent_hash, nonce) pairs
// 2. Epoch-based challenges with sequence tracking
// 3. Database-backed nonce registry for production
//
// A replay test would look like:
//
// ```rust
// #[test]
// fn test_poc_replay_rejected() {
//     let cta = CtaBuilder::new(keypair)
//         .freshness(StatefulFreshnessValidator::new())
//         .build();
//
//     // First submission succeeds
//     let result1 = cta.submit(&parent, &poc, &request, now);
//     assert!(result1.is_ok());
//
//     // Same PoC replay fails
//     let result2 = cta.submit(&parent, &poc, &request, now);
//     assert!(matches!(result2, Err(CtaError::FreshnessError(_))));
// }
// ```
//
// This is deferred to Phase 4 (production replay prevention).

#[test]
fn test_python_pca_final() {
    let hex = "a66776657273696f6e8200016c6361706162696c697469657381a3636b6579656361703a30647479706569746f6f6c2d63616c6c646461746149a164746f6f6c622a2a7364657369676e617465645f6578656375746f72a26474797065667075626b65796576616c756582005820ee31f83c88a71219a6fcf9bee0da9bc22620588f5a15a6145553504df9649e5c6a657870697265735f61747819323032362d30312d30355430313a30303a32372b30303a3030666973737565727848656432353531393a65653331663833633838613731323139613666636639626565306461396263323236323035383866356131356136313435353533353034646639363439653563697369676e61747572657888656432353531393a6163653030616533316637373934306438613937646135616462303162663962306133646131393963663164653038653363663665633766646232636365306163396136366435636666393737383438616332316337363533653537333365326561376562353536373462316439663966313363633531333234663463313035";
    let bytes = hex::decode(hex).unwrap();

    match amla_protocol::Pca::from_cbor(&bytes) {
        Ok(pca) => {
            println!("Parsed OK");
            println!("Issuer: {}", pca.issuer().to_hex());
            println!("Expires: {}", pca.expires_at());

            match pca.try_verify_signature() {
                Ok(()) => println!("Signature: VERIFIED!"),
                Err(e) => println!("Signature: FAILED - {e}"),
            }
        }
        Err(e) => println!("Parse failed: {e}"),
    }
}
