use crate::components::{
    memory::{
        self,
        component::{MemoryComponent, MemoryEval},
        table::{interaction_trace_evaluation, MemoryElements, MemoryTable},
    },
    MemoryClaim,
};
use brainfuck_vm::machine::Machine;
use stwo_prover::{
    constraint_framework::{
        preprocessed_columns::PreprocessedColumn, TraceLocationAllocator, INTERACTION_TRACE_IDX,
        ORIGINAL_TRACE_IDX, PREPROCESSED_TRACE_IDX,
    },
    core::{
        air::{Component, ComponentProver},
        backend::simd::SimdBackend,
        channel::{Blake2sChannel, Channel},
        pcs::{CommitmentSchemeProver, CommitmentSchemeVerifier, PcsConfig, TreeVec},
        poly::circle::{CanonicCoset, PolyOps},
        prover::{self, verify, ProvingError, StarkProof, VerificationError},
        vcs::{
            blake2_merkle::{Blake2sMerkleChannel, Blake2sMerkleHasher},
            ops::MerkleHasher,
        },
    },
};

/// The STARK proof of the execution of a given Brainfuck program.
///
/// It includes the proof as well as the claims during the various phases of the proof generation.
pub struct BrainfuckProof<H: MerkleHasher> {
    pub claim: BrainfuckClaim,
    pub interaction_claim: BrainfuckInteractionClaim,
    pub proof: StarkProof<H>,
}

/// All the claims from the first phase (interaction phase 0).
///
/// It includes the common claim values such as the initial and final states
/// and the claim of each component.
pub struct BrainfuckClaim {
    pub memory: MemoryClaim,
}

impl BrainfuckClaim {
    pub fn mix_into(&self, channel: &mut impl Channel) {
        self.memory.mix_into(channel);
    }

    pub fn log_sizes(&self) -> TreeVec<Vec<u32>> {
        self.memory.log_sizes()
    }
}

/// All the interaction elements (drawn from the channel)
/// required by the various components during the interaction phase.
pub struct BrainfuckInteractionElements {
    pub memory_lookup_elements: MemoryElements,
}

impl BrainfuckInteractionElements {
    /// Draw all the interaction elements needed for
    /// all the components of the Brainfuck ZK-VM.
    pub fn draw(channel: &mut impl Channel) -> Self {
        Self { memory_lookup_elements: MemoryElements::draw(channel) }
    }
}

/// All the claims from the second phase (interaction phase 2).
///
/// Mainly the claims on global relations (lookup, permutation, evaluation).
pub struct BrainfuckInteractionClaim {
    memory: memory::component::InteractionClaim,
}

impl BrainfuckInteractionClaim {
    /// Mix the claimed sums of every components in the Fiat-Shamir [`Channel`].
    pub fn mix_into(&self, channel: &mut impl Channel) {
        self.memory.mix_into(channel);
    }
}

/// Verify that the claims (i.e. Statement) are valid.
pub fn lookup_sum_valid(
    _claim: &BrainfuckClaim,
    _interaction_elements: &BrainfuckInteractionElements,
    _interaction_claim: &BrainfuckInteractionClaim,
) -> bool {
    todo!();
}

/// All the components that constitute the Brainfuck ZK-VM.
///
/// Components are used by the prover as a `ComponentProver`,
/// and by the verifier as a `Component`.
pub struct BrainfuckComponents {
    memory: MemoryComponent,
}

impl BrainfuckComponents {
    /// Initializes all the Brainfuck components from the claims generated from the trace.
    pub fn new(
        claim: &BrainfuckClaim,
        interaction_elements: &BrainfuckInteractionElements,
        interaction_claim: &BrainfuckInteractionClaim,
    ) -> Self {
        let memory_is_first_column = PreprocessedColumn::IsFirst(claim.memory.log_size);

        let tree_span_provider =
            &mut TraceLocationAllocator::new_with_preproccessed_columnds(&[memory_is_first_column]);

        let memory = MemoryComponent::new(
            tree_span_provider,
            MemoryEval::new(
                &claim.memory,
                interaction_elements.memory_lookup_elements.clone(),
                &interaction_claim.memory,
            ),
            (interaction_claim.memory.claimed_sum, None),
        );

        Self { memory }
    }

    /// Returns the `ComponentProver` of each components, used by the prover.
    pub fn provers(&self) -> Vec<&dyn ComponentProver<SimdBackend>> {
        vec![&self.memory]
    }

    /// Returns the `Component` of each components, used by the verifier.
    pub fn components(&self) -> Vec<&dyn Component> {
        self.provers().into_iter().map(|component| component as &dyn Component).collect()
    }
}

/// `LOG_MAX_ROWS = ilog2(MAX_ROWS)`
///
/// Means that the ZK-VM does not accept programs with more than 2^20 steps (1M steps).
const LOG_MAX_ROWS: u32 = 20;

/// Generate a STARK proof of the given Brainfuck program execution.
///
/// # Arguments
/// * `inputs` - The [`Machine`] struct after the program execution
/// The inputs contains the program, the memory, the I/O and the trace.
pub fn prove_brainfuck(
    inputs: &Machine,
) -> Result<BrainfuckProof<Blake2sMerkleHasher>, ProvingError> {
    // ┌──────────────────────────┐
    // │     Protocol Setup       │
    // └──────────────────────────┘

    let config = PcsConfig::default();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(LOG_MAX_ROWS + config.fri_config.log_blowup_factor + 2)
            .circle_domain()
            .half_coset,
    );
    let channel = &mut Blake2sChannel::default();
    let commitment_scheme =
        &mut CommitmentSchemeProver::<_, Blake2sMerkleChannel>::new(config, &twiddles);

    // ┌───────────────────────────────────────────────┐
    // │   Interaction Phase 0 - Preprocessed Trace    │
    // └───────────────────────────────────────────────┘

    // Generate constant columns (e.g. is_first)
    let tree_builder = commitment_scheme.tree_builder();
    tree_builder.commit(channel);

    // ┌───────────────────────────────────────┐
    // │    Interaction Phase 1 - Main Trace   │
    // └───────────────────────────────────────┘

    let mut tree_builder = commitment_scheme.tree_builder();

    let vm_trace = inputs.trace();
    let (memory_trace, memory_claim) = MemoryTable::from(vm_trace).trace_evaluation().unwrap();

    tree_builder.extend_evals(memory_trace.clone());

    let claim = BrainfuckClaim { memory: memory_claim };

    // Mix the claim into the Fiat-Shamir channel.
    claim.mix_into(channel);
    // Commit the main trace.
    tree_builder.commit(channel);

    // ┌───────────────────────────────────────────────┐
    // │    Interaction Phase 2 - Interaction Trace    │
    // └───────────────────────────────────────────────┘

    // Draw interaction elements
    let interaction_elements = BrainfuckInteractionElements::draw(channel);

    // Generate the interaction trace and the BrainfuckInteractionClaim
    let mut tree_builder = commitment_scheme.tree_builder();

    let (memory_interaction_trace_eval, memory_interaction_claim) =
        interaction_trace_evaluation(&memory_trace, &interaction_elements.memory_lookup_elements);

    tree_builder.extend_evals(memory_interaction_trace_eval);

    let interaction_claim = BrainfuckInteractionClaim { memory: memory_interaction_claim };

    // Mix the interaction claim into the Fiat-Shamir channel.
    interaction_claim.mix_into(channel);
    // Commit the interaction trace.
    tree_builder.commit(channel);

    // ┌──────────────────────────┐
    // │     Proof Generation     │
    // └──────────────────────────┘
    let component_builder =
        BrainfuckComponents::new(&claim, &interaction_elements, &interaction_claim);
    let components = component_builder.provers();
    let proof = prover::prove::<SimdBackend, _>(&components, channel, commitment_scheme)?;

    Ok(BrainfuckProof { claim, interaction_claim, proof })
}

/// Verify a given STARK proof of a Brainfuck program execution with corresponding claim.
pub fn verify_brainfuck(
    BrainfuckProof { claim, interaction_claim, proof }: BrainfuckProof<Blake2sMerkleHasher>,
) -> Result<(), VerificationError> {
    // ┌──────────────────────────┐
    // │     Protocol Setup       │
    // └──────────────────────────┘
    let config = PcsConfig::default();
    let channel = &mut Blake2sChannel::default();
    let commitment_scheme_verifier =
        &mut CommitmentSchemeVerifier::<Blake2sMerkleChannel>::new(config);
    let log_sizes = &claim.log_sizes();

    // ┌───────────────────────────────────────────────┐
    // │   Interaction Phase 0 - Preprocessed Trace    │
    // └───────────────────────────────────────────────┘

    commitment_scheme_verifier.commit(
        proof.commitments[PREPROCESSED_TRACE_IDX],
        &log_sizes[PREPROCESSED_TRACE_IDX],
        channel,
    );

    // ┌───────────────────────────────────────┐
    // │    Interaction Phase 1 - Main Trace   │
    // └───────────────────────────────────────┘
    claim.mix_into(channel);
    commitment_scheme_verifier.commit(
        proof.commitments[ORIGINAL_TRACE_IDX],
        &log_sizes[ORIGINAL_TRACE_IDX],
        channel,
    );

    // ┌───────────────────────────────────────────────┐
    // │    Interaction Phase 2 - Interaction Trace    │
    // └───────────────────────────────────────────────┘

    let interaction_elements = BrainfuckInteractionElements::draw(channel);
    // Check that the lookup sum is valid, otherwise throw
    if !lookup_sum_valid(&claim, &interaction_elements, &interaction_claim) {
        return Err(VerificationError::InvalidLookup("Invalid LogUp sum".to_string()));
    };
    interaction_claim.mix_into(channel);
    commitment_scheme_verifier.commit(
        proof.commitments[INTERACTION_TRACE_IDX],
        &log_sizes[INTERACTION_TRACE_IDX],
        channel,
    );

    // ┌──────────────────────────┐
    // │    Proof Verification    │
    // └──────────────────────────┘

    let component_builder =
        BrainfuckComponents::new(&claim, &interaction_elements, &interaction_claim);
    let components = component_builder.components();

    verify(&components, channel, commitment_scheme_verifier, proof)
}
