pub mod block_appendix;
pub mod block_body;
pub mod block_header;
pub mod block_height;
pub mod block_info;
pub mod block_kernel;
pub mod block_selector;
mod block_validation_error;
pub mod difficulty_control;
pub mod mock_block_generator;
pub mod mutator_set_update;
pub mod validity;

use std::sync::Arc;
use std::sync::OnceLock;

use block_appendix::BlockAppendix;
use block_appendix::MAX_NUM_CLAIMS;
use block_body::BlockBody;
use block_header::BlockHeader;
use block_header::ADVANCE_DIFFICULTY_CORRECTION_FACTOR;
use block_header::ADVANCE_DIFFICULTY_CORRECTION_WAIT;
use block_height::BlockHeight;
use block_kernel::BlockKernel;
use block_validation_error::BlockValidationError;
use difficulty_control::Difficulty;
use get_size2::GetSize;
use itertools::Itertools;
use mutator_set_update::MutatorSetUpdate;
use num_traits::CheckedSub;
use num_traits::Zero;
use serde::Deserialize;
use serde::Serialize;
use tasm_lib::triton_vm::prelude::*;
use tasm_lib::twenty_first::util_types::mmr::mmr_accumulator::MmrAccumulator;
use tasm_lib::twenty_first::util_types::mmr::mmr_trait::Mmr;
use tracing::warn;
use twenty_first::math::b_field_element::BFieldElement;
use twenty_first::math::bfield_codec::BFieldCodec;
use twenty_first::math::digest::Digest;
use validity::block_primitive_witness::BlockPrimitiveWitness;
use validity::block_program::BlockProgram;
use validity::block_proof_witness::BlockProofWitness;

use super::transaction::transaction_kernel::TransactionKernelProxy;
use super::transaction::utxo::Utxo;
use super::transaction::Transaction;
use super::type_scripts::native_currency_amount::NativeCurrencyAmount;
use super::type_scripts::time_lock::TimeLock;
use crate::api::tx_initiation::builder::proof_builder::ProofBuilder;
use crate::config_models::network::Network;
use crate::models::blockchain::block::difficulty_control::difficulty_control;
use crate::models::blockchain::shared::Hash;
use crate::models::blockchain::transaction::utxo::Coin;
use crate::models::blockchain::transaction::validity::neptune_proof::Proof;
use crate::models::blockchain::transaction::validity::single_proof::SingleProof;
use crate::models::proof_abstractions::mast_hash::MastHash;
use crate::models::proof_abstractions::tasm::program::ConsensusProgram;
use crate::models::proof_abstractions::tasm::program::TritonVmProofJobOptions;
use crate::models::proof_abstractions::timestamp::Timestamp;
use crate::models::proof_abstractions::verifier::verify;
use crate::models::proof_abstractions::SecretWitness;
use crate::models::state::wallet::address::hash_lock_key::HashLockKey;
use crate::models::state::wallet::address::ReceivingAddress;
use crate::models::state::wallet::wallet_entropy::WalletEntropy;
use crate::prelude::twenty_first;
use crate::triton_vm_job_queue::TritonVmJobQueue;
use crate::util_types::mutator_set::addition_record::AdditionRecord;
use crate::util_types::mutator_set::commit;
use crate::util_types::mutator_set::mutator_set_accumulator::MutatorSetAccumulator;

/// Block height for 1st hardfork that increases block size limit to allow for
/// more inputs per transaction.
pub(crate) const BLOCK_HEIGHT_HF_1: BlockHeight = BlockHeight::new(BFieldElement::new(6_000));

/// Old maximum block size in number of `BFieldElement`s.
pub(crate) const MAX_BLOCK_SIZE_BEFORE_HF_1: usize = 250_000;

/// New maximum block size in number of `BFieldElement`s.
///
/// This size is 8MB which should keep it feasible to run archival nodes for
/// many years without requiring excessive disk space. With an SWBF MMR of
/// height 20, this limit allows for 150-200 inputs per block.
pub(crate) const MAX_BLOCK_SIZE_AFTER_HF_1: usize = 1_000_000;

/// With removal records only represented by their absolute index set, the block
/// size limit of 1.000.000 `BFieldElement`s allows for a "balanced" block
/// (equal number of inputs and outputs, no public announcements) of ~10.000
/// input and outputs. To prevent an attacker from making it costly to run an
/// archival node, the number of outputs is restricted. For simplicity though
/// this limit is enforced for inputs, outputs, and public announcements. This
/// restriction on the number of public announcements also makes it feasible for
/// wallets to scan through all.
const MAX_NUM_INPUTS_OUTPUTS_PUB_ANNOUNCEMENTS_AFTER_HF_1: usize = 1 << 14;

/// Duration of timelock for half of all mining rewards.
///
/// Half the block subsidy is liquid immediately. Half of it is locked for this
/// time period. Likewise, half the guesser fee is liquid immediately; and half
/// is time locked for this period.
pub(crate) const MINING_REWARD_TIME_LOCK_PERIOD: Timestamp = Timestamp::years(3);

pub(crate) const INITIAL_BLOCK_SUBSIDY: NativeCurrencyAmount = NativeCurrencyAmount::coins(128);

/// All blocks have proofs except the genesis block
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BFieldCodec, GetSize, Default)]
pub enum BlockProof {
    Genesis,
    #[default]
    Invalid,
    SingleProof(Proof),
}

/// Public fields of `Block` are read-only, enforced by #[readonly::make].
/// Modifications are possible only through `Block` methods.
///
/// Example:
///
/// test: verify that compile fails on an attempt to mutate block
/// internals directly (bypassing encapsulation)
///
/// ```compile_fail,E0594
/// use neptune_cash::models::blockchain::block::Block;
/// use neptune_cash::config_models::network::Network;
/// use neptune_cash::prelude::twenty_first::math::b_field_element::BFieldElement;
/// use tasm_lib::prelude::Digest;
///
/// let mut block = Block::genesis(Network::RegTest);
///
/// let height = block.kernel.header.height;
///
/// let nonce = Digest::default();
///
/// // this line fails to compile because we try to
/// // mutate an internal field.
/// block.kernel.header.nonce = nonce;
/// ```
// ## About the private `digest` field:
//
// The `digest` field represents the `Block` hash.  It is an optimization so
// that the hash can be lazily computed at most once (per modification).
//
// It is wrapped in `OnceLock<_>` for interior mutability because (a) the hash()
// method is used in many methods that are `&self` and (b) because `Block` is
// passed between tasks/threads, and thus `Rc<RefCell<_>>` is not an option.
//
// The field must be reset whenever the Block is modified.  As such, we should
// not permit direct modification of internal fields, particularly `kernel`
//
// Therefore `[readonly::make]` is used to make public `Block` fields read-only
// (not mutable) outside of this module.  All methods that modify Block also
// reset the `digest` field.
//
// We manually implement `PartialEq` and `Eq` so that digest field will not be
// compared.  Otherwise, we could have identical blocks except one has
// initialized digest field and the other has not.
//
// The field should not be serialized, so it has the `#[serde(skip)]` attribute.
// Upon deserialization, the field will have Digest::default() which is desired
// so that the digest will be recomputed if/when hash() is called.
//
// We likewise skip the field for `BFieldCodec`, and `GetSize` because there
// exist no impls for `OnceLock<_>` so derive fails.
//
// A unit test-suite exists in module tests::digest_encapsulation.
#[readonly::make]
#[derive(Debug, Clone, Serialize, Deserialize, BFieldCodec, GetSize)]
pub struct Block {
    /// Everything but the proof
    pub kernel: BlockKernel,

    pub proof: BlockProof,

    // this is only here as an optimization for Block::hash()
    // so that we lazily compute the hash at most once.
    #[serde(skip)]
    #[bfield_codec(ignore)]
    #[get_size(ignore)]
    digest: OnceLock<Digest>,
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        // TBD: is it faster overall to compare hashes or equality
        // of kernel and blocktype fields?
        // In the (common?) case where hash has already been
        // computed for both `Block` comparing hash equality
        // should be faster.
        self.hash() == other.hash()
    }
}
impl Eq for Block {}

impl Block {
    /// Create a block template with an invalid block proof, from a block
    /// primitive witness.
    #[cfg(test)]
    pub(crate) fn block_template_invalid_proof_from_witness(
        primitive_witness: BlockPrimitiveWitness,
        block_timestamp: Timestamp,
        target_block_interval: Timestamp,
    ) -> Block {
        let body = primitive_witness.body().to_owned();
        let header = primitive_witness.header(block_timestamp, target_block_interval);
        let proof = BlockProof::Invalid;
        let appendix = BlockAppendix::default();

        Block::new(header, body, appendix, proof)
    }

    /// Create a block template with an invalid block proof.
    ///
    /// To be used in tests where you don't care about block validity.
    #[cfg(test)]
    pub(crate) fn block_template_invalid_proof(
        predecessor: &Block,
        transaction: Transaction,
        block_timestamp: Timestamp,
        target_block_interval: Timestamp,
    ) -> Block {
        let primitive_witness = BlockPrimitiveWitness::new(predecessor.to_owned(), transaction);
        Self::block_template_invalid_proof_from_witness(
            primitive_witness,
            block_timestamp,
            target_block_interval,
        )
    }

    pub(crate) async fn block_template_from_block_primitive_witness(
        primitive_witness: BlockPrimitiveWitness,
        timestamp: Timestamp,
        triton_vm_job_queue: Arc<TritonVmJobQueue>,
        proof_job_options: TritonVmProofJobOptions,
    ) -> anyhow::Result<Block> {
        let network = proof_job_options.job_settings.network;
        let body = primitive_witness.body().to_owned();
        let header = primitive_witness.header(timestamp, network.target_block_interval());
        let (appendix, proof) = {
            let block_proof_witness = BlockProofWitness::produce(primitive_witness);
            let appendix = block_proof_witness.appendix();
            let claim = BlockProgram::claim(&body, &appendix);

            let proof = ProofBuilder::new()
                .program(BlockProgram.program())
                .claim(claim)
                .nondeterminism(|| block_proof_witness.nondeterminism())
                .job_queue(triton_vm_job_queue)
                .proof_job_options(proof_job_options)
                .build()
                .await?;

            (appendix, BlockProof::SingleProof(proof))
        };

        Ok(Block::new(header, body, appendix, proof))
    }

    async fn make_block_template_with_valid_proof(
        predecessor: &Block,
        transaction: Transaction,
        block_timestamp: Timestamp,
        triton_vm_job_queue: Arc<TritonVmJobQueue>,
        proof_job_options: TritonVmProofJobOptions,
    ) -> anyhow::Result<Block> {
        let network = proof_job_options.job_settings.network;
        let tx_claim = SingleProof::claim(transaction.kernel.mast_hash());
        assert!(
            verify(
                tx_claim.clone(),
                transaction.proof.clone().into_single_proof().clone(),
                network,
            )
            .await,
            "Transaction proof must be valid to generate a block"
        );
        assert!(
            transaction.kernel.merge_bit,
            "Merge-bit must be set in transactions before they can be included in blocks."
        );
        let primitive_witness = BlockPrimitiveWitness::new(predecessor.to_owned(), transaction);
        Self::block_template_from_block_primitive_witness(
            primitive_witness,
            block_timestamp,
            triton_vm_job_queue,
            proof_job_options,
        )
        .await
    }

    /// Compose a block.
    ///
    /// Create a block with valid block proof, but without proof-of-work.
    pub(crate) async fn compose(
        predecessor: &Block,
        transaction: Transaction,
        block_timestamp: Timestamp,
        triton_vm_job_queue: Arc<TritonVmJobQueue>,
        proof_job_options: TritonVmProofJobOptions,
    ) -> anyhow::Result<Block> {
        Self::make_block_template_with_valid_proof(
            predecessor,
            transaction,
            block_timestamp,
            triton_vm_job_queue,
            proof_job_options,
        )
        .await
    }

    /// Returns the block Digest
    ///
    /// performance note:
    ///
    /// The digest is never computed until hash() is called.  Subsequent calls
    /// will not recompute it unless the Block was modified since the last call.
    #[inline]
    pub fn hash(&self) -> Digest {
        *self.digest.get_or_init(|| self.kernel.mast_hash())
    }

    #[inline]
    fn unset_digest(&mut self) {
        // note: this replaces the OnceLock so the digest will be calc'd in hash()
        self.digest = Default::default();
    }

    /// sets header nonce.
    ///
    /// note: this causes block digest to change.
    #[inline]
    pub fn set_header_nonce(&mut self, nonce: Digest) {
        self.kernel.header.nonce = nonce;
        self.unset_digest();
    }

    /// Set the guesser digest in the block's header.
    ///
    /// Note: this causes the block digest to change.
    #[inline]
    pub(crate) fn set_header_guesser_digest(&mut self, guesser_after_image: Digest) {
        self.kernel.header.guesser_digest = guesser_after_image;
        self.unset_digest();
    }

    /// sets header timestamp and difficulty.
    ///
    /// These must be set as a pair because the difficulty depends
    /// on the timestamp, and may change with it.
    ///
    /// note: this causes block digest to change.
    #[inline]
    pub(crate) fn set_header_timestamp_and_difficulty(
        &mut self,
        timestamp: Timestamp,
        difficulty: Difficulty,
    ) {
        self.kernel.header.timestamp = timestamp;
        self.kernel.header.difficulty = difficulty;

        self.unset_digest();
    }

    #[inline]
    pub fn header(&self) -> &BlockHeader {
        &self.kernel.header
    }

    #[inline]
    pub fn body(&self) -> &BlockBody {
        &self.kernel.body
    }

    /// Return the mutator set as it looks after the application of this block.
    ///
    /// Includes the guesser-fee UTXOs which are not included by the
    /// `mutator_set_accumulator` field on the block body.
    pub fn mutator_set_accumulator_after(
        &self,
    ) -> Result<MutatorSetAccumulator, BlockValidationError> {
        let mut msa = self.kernel.body.mutator_set_accumulator.clone();
        let mutator_set_update =
            MutatorSetUpdate::new(vec![], self.guesser_fee_addition_records()?);
        mutator_set_update.apply_to_accumulator(&mut msa)
            .expect("mutator set update derived from guesser fees should be applicable to mutator set accumulator contained in body");

        Ok(msa)
    }

    #[inline]
    pub(crate) fn appendix(&self) -> &BlockAppendix {
        &self.kernel.appendix
    }

    /// note: this causes block digest to change to that of the new block.
    #[inline]
    pub fn set_block(&mut self, block: Block) {
        *self = block;
    }

    /// The number of coins that can be printed into existence with the mining
    /// a block with this height.
    pub fn block_subsidy(block_height: BlockHeight) -> NativeCurrencyAmount {
        let mut reward: NativeCurrencyAmount = INITIAL_BLOCK_SUBSIDY;
        let generation = block_height.get_generation();

        for _ in 0..generation {
            reward.div_two();

            // Early return here is important bc of test-case generators with
            // arbitrary block heights.
            if reward.is_zero() {
                return NativeCurrencyAmount::zero();
            }
        }

        reward
    }

    /// returns coinbase reward amount for this block.
    ///
    /// note that this amount may differ from self::block_subsidy(self.height)
    /// because a miner can choose to accept less than the calculated reward amount.
    pub fn coinbase_amount(&self) -> NativeCurrencyAmount {
        // A block must always have a Coinbase.
        // we impl this method in part to cement that guarantee.
        self.body()
            .transaction_kernel
            .coinbase
            .unwrap_or_else(NativeCurrencyAmount::zero)
    }

    pub fn genesis(network: Network) -> Self {
        let premine_distribution = Self::premine_distribution();
        let total_premine_amount = premine_distribution
            .iter()
            .map(|(_receiving_address, amount)| *amount)
            .sum();

        let mut ms_update = MutatorSetUpdate::default();
        let mut genesis_mutator_set = MutatorSetAccumulator::default();
        let mut genesis_tx_outputs = vec![];
        for ((receiving_address, _amount), utxo) in premine_distribution
            .iter()
            .zip(Self::premine_utxos(network))
        {
            let utxo_digest = Hash::hash(&utxo);
            // generate randomness for mutator set commitment
            // Sender randomness cannot be random because there is no sender.
            let bad_randomness = Self::premine_sender_randomness(network);

            let receiver_digest = receiving_address.privacy_digest();

            // Add pre-mine UTXO to MutatorSet
            let addition_record = commit(utxo_digest, bad_randomness, receiver_digest);
            ms_update.additions.push(addition_record);
            genesis_mutator_set.add(&addition_record);

            // Add pre-mine UTXO + commitment to coinbase transaction
            genesis_tx_outputs.push(addition_record)
        }

        let genesis_txk = TransactionKernelProxy {
            inputs: vec![],
            outputs: genesis_tx_outputs,
            fee: NativeCurrencyAmount::coins(0),
            timestamp: network.launch_date(),
            public_announcements: vec![],
            coinbase: Some(total_premine_amount),
            mutator_set_hash: MutatorSetAccumulator::default().hash(),
            merge_bit: false,
        }
        .into_kernel();

        let body: BlockBody = BlockBody::new(
            genesis_txk,
            genesis_mutator_set.clone(),
            MmrAccumulator::new_from_leafs(vec![]),
            MmrAccumulator::new_from_leafs(vec![]),
        );

        let header = BlockHeader::genesis(network);

        let appendix = BlockAppendix::default();

        Self::new(header, body, appendix, BlockProof::Genesis)
    }

    /// sender randomness is tailored to the network. This change
    /// percolates into the mutator set hash and eventually into all transaction
    /// kernels. The net result is that broadcasting transaction on other
    /// networks invalidates the lock script proofs.
    pub(crate) fn premine_sender_randomness(network: Network) -> Digest {
        Digest::new(bfe_array![network as u64, 0, 0, 0, 0])
    }

    fn premine_distribution() -> Vec<(ReceivingAddress, NativeCurrencyAmount)> {
        // The premine UTXOs can be hardcoded here.
        let authority_wallet = WalletEntropy::devnet_wallet();
        let authority_receiving_address = authority_wallet
            .nth_generation_spending_key(0)
            .to_address()
            .into();
        vec![
            // chiefly for testing; anyone can access these coins by generating
            // the devnet wallet as above
            (authority_receiving_address, NativeCurrencyAmount::coins(20)),

            // Legacy address (for testing), generated on alphanet v0.5.0
            (ReceivingAddress::from_bech32m("nolgam1lf8vc5xpa4jf9vjakts632fct5q80d4m6tax39nrl8c55dta2h7n7lnkh9pmwckl0ndwc7897xwfgx5vv02xdt3099z62222wazz7tjl6umzewla9xzxyqefh2w47v4eh0xzvfsxjk6kq5u84rwwlflq7cs726ljttl6ls860te04cwpy5kk8n40qqjnps0gdp46namhsa3cqt0uc0s5e34h6s5rw2kl77uvvs4rlnn5t8wtuefsduuccwsxmk27r8d48g49swgafhj6wmvu5cx3lweqhnxgdgm7mmdq7ck6wkurw2jzl64k9u34kzgu9stgd47ljzte0hz0n2lcng83vtpf0u9f4hggw4llqsz2fqpe4096d9v5fzg7xvxg6zvr7gksq4yqgn8shepg5xsczmzz256m9c6r8zqdkzy4tk9he59ndtdkrrr8u5v6ztnvkvmy4sed7p7plm2y09sgksw6zcjayls4wl9fnqu97kyx9cdknksar7h8jetygur979rt5arcwmvp2dy3ynt6arna2yjpevt9209v9g2p5cvp6gjp9850w3w6afeg8yuhp6u447hrudcssyjauqa2p7jk4tz37wg70yrdhsgn35sc0hdkclvpapu75dgtmswk0vtgadx44mqdps6ry6005xqups9dpc93u66qj9j7lfaqgdqrrfg9pkxhjl99ge387rh257x2phfvjvc8y66p22wax8myyhm7mgmlxu9gug0km3lmn4lzcyj32mduy6msy4kfn5z2tr67zfxadnj6wc0av27mk0j90pf67uzp9ps8aekr24kpv5n3qeczfznen9vj67ft95s93t26l8uh87qr6kp8lsyuzm4h36de830h6rr3lhg5ac995nrsu6h0p56t5tnglvx0s02mr0ts95fgcevveky5kkw6zgj6jd5m3n5ljhw862km8sedr30xvg8t9vh409ufuxdnfuypvqdq49z6mp46p936pjzwwqjda6yy5wuxx9lffrxwcmfqzch6nz2l4mwd2vlsdr58vhygppy6nm6tduyemw4clwj9uac4v990xt6jt7e2al7m6sjlq4qgxfjf4ytx8f5j460vvr7yac9hsvlsat2vh5gl55mt4wr7v5p3m6k5ya5442xdarastxlmpf2vqz5lusp8tlglxkj0jksgwqgtj6j0kxwmw40egpzs5rr996xpv8wwqyja4tmw599n9fh77f5ruxk69vtpwl9z5ezmdn92cpyyhwff59ypp0z5rv98vdvm67umqzt0ljjan30u3a8nga35fdy450ht9gef24mveucxqwv5aflge5r3amxsvd7l30j9kcqm7alq0ks2wqpde7pdct2gmvafxvjg3ad0a3h58assjaszvmykl3k5tn238gstm2shlvad4a53mm5ztvp5q2zt4pdzj0ssevlkumwhc0g5cxnxc9u7rh9gffkq7h9ufcxkgtghe32sv3vwzkessr52mcmajt83lvz45wqru9hht8cytfedtjlv7z7en6pp0guja85ft3rv6hzf2e02e7wfu38s0nyfzkc2qy2k298qtmxgrpduntejtvenr80csnckajnhu44399tkm0a7wdldalf678n9prd54twwlw24xhppxqlquatfztllkeejlkfxuayddwagh6uzx040tqlcs7hcflnu0ywynmz0chz48qcx7dsc4gpseu0dqvmmezpuv0tawm78nleju2vp4lkehua56hrnuj2wuc5lqvxlnskvp53vu7e2399pgp7xcwe3ww23qcd9pywladq34nk6cwcvtj3vdfgwf6r7s6vq46y2x05e043nj6tu8am2und8z3ftf3he5ccjxamtnmxfd79m04ph36kzx6e789dhqrwmwcfrn9ulsedeplk3dvrmad6f20y9qfl6n6kzaxkmmmaq4d6s5rl4kmhc7fcdkrkandw2jxdjckuscu56syly8rtjatj4j2ug23cwvep3dgcdvmtr32296nf9vdl3rcu0r7hge23ydt83k5nhtnexuqrnamveacz6c43eay9nz4pjjwjatkgp80lg9tnf5kdr2eel8s2fk6v338x4hu00htemm5pq6qlucqqq5tchhtekjzdu50erqd2fkdu9th3wl0mqxz5u7wnpgwgpammv2yqpa5znljegyhke0dz9vg27uh5t5x6qdgf7vu54lqssejekwzfxchjyq2s8frm9fmt688w76aug56v6n3w5xdre78xplfsdw3e4j6dc5w7tf83r25re0duq6h8z54wnkqr9yh2k0skjqea4elgcr4aw7hks9m8w3tx8w9xlxpqqll2zeql55ew7e90dyuynkqxfuqzv45t22ljamdll3udvqrllprdltthzm866jdaxkkrnryj4cmc2m7sk99clgql3ynrhe9kynqn4mh3tepk8dtq7cndtc2hma29s4cuylsvg04s70uyr53w5656su5rjem5egss08zrfaef0mww6t8pr26uph2n8a2cs55ydx4xhasjqk7xs0akh6f26j2ec4d8pd0kdf4jya6p9jl48wmy5autdpw2q8mehrq6kypt573genj66l5zkq6xvrdqugmfczxa2gj9ylx3pgpjqnhuem9udfkj9qr2y8lh728sr7uaedu5wwmfa72ykh395jqh7f7f9p2gskn6u7k844kpnwe3eqv84pl53r6x9af88a8ey7298njdg03h8mxqz2x6z8ys3qpuxq768tjq0zhrnjgns8d78euzwsvx6vn4f9tftrp68zcch3h75mc9drpt7tpvnyyqfjuqclxhdwhdwtsakecv04p9r3jx90htql9a3ht5mxrj4ercv4cd52wk4qhu7dn4tqe7yclqx2l36gcsrzmdlv440qls7qjpq6k95mst485vpennnur8h62a7d7syvyer89qtyfzlfhz8a5a0x5tuwhc9mah0e944xzhsc6uvpv8vat44w7r3xyw8q85y77jux8zhndrhdn36swryffqmpkxgcw4g29q40sul4fl5vrfru08a5j3rd3jl8799srpf2xqpxq38wwvhr4mxqf5wwdqfqq7harshggvufzlgn0l9fq0j76dyuge75jmzy8celvw6wesfs82n4jw2k8jnus2zds5a67my339uuzka4w72tau6j7wyu0lla0mcjpaflphsuy7f2phev6tr8vc9nj2mczkeg4vy3n5jkgecwgrvwu3vw9x5knpkxzv8kw3dpzzxy3rvrs56vxw8ugmyz2vdj6dakjyq3feym4290l7hgdt0ac5u49sekezzf0ghwmlek4h75fkzpvuly9zupw32dd3l9my282nekgk78fe6ayjyhczetxf8r82yd2askl52kmupr9xaxw0jd08dsd3523ea6ge48384rlmt4mu4w4x0q9s", Network::Main).unwrap(), NativeCurrencyAmount::coins(1)),

            // Legacy address (for testing), generated on betanet v0.10.0
            (ReceivingAddress::from_bech32m("nolgam19ch0269tvlvvamk7em5mhtpja3pe8tm58dmzegy8psnkq8ezqtltw7ykxlcjh5fgjgrgcwcnshpy6ulcyjdg24ncfu7q956cc0knrhgju3spvemslp5d7tncd9n5mxfq2yrhzjlpnrrr65qd4a3kyj9f52gs6m4f7am0at96rx5uez9unm4d2a4chvtpp0wa5ewjxrs2stwv79vfqaes6qep2vcvg4hfcv937hj0cs7eng9f396z57mtxfscmkjvh675zy0pdx577taj7en80x47heufykth59waue2rshqu3r7hfna67uk224exep60smfr8xch0f20ay7gw7r0nyx79ndzge8r893xsk3ksqravln2j74jrxadkl0tkljc3z0ynwzae7f8drmfmp2gtja94hx764hsf9tfsakj4av67hw6ey7u48wsqkmnvflznuvkn8f3xxl9w3dk4fvv2wqx5h7ystz5p9j0l8q0r3tzp42ehvfwaxl5vlwwvv5l9yzwjj2wlmttteghqn5563j4u2dqmr5p0tskg083ecv4p9w7l2vgl63m95q29uhjlmu2ktq7fdj9pmw2svtwwhekhz9ljxsk0mdajyhy0a4znz9sswe86ncx82g5pa69vy0p5elqu8rljeh0y7hm73et6pzrfwkuywet5pmf03qsyma3s3kw07zhmxrajxl92chfyxm6jttqpcm7zh3djmdxkpj9y3eecha3jvu58h88qnym6475v3q466yhtsnxglznupu9jmp0cd3zs0nt4s77jsdq4s5gmepx6yt76n5kt2j666tql4u9cz8s5ua6qu4e2qcdk4jxep94u680434yvr4jklqnxveu9ywq7a8lhk4rk6hdhhmr3me8ajcqtweumdjtst7a6l7sprvly6m7rm9u4n69un8slyjk2ljphu4t2ay34zg0n5e6p0hnwqdcxm8yxcruc5cfcl7cf04smzq6tu26ael5mwz0857v7scy2vf6v4aj6akdx2q0d87uf7q9yrylqkmay4cw6upnncyy3rhxve4qzt86gc5qx6jhw79lstv0wthh6q3pfs5hqvafq6pgfewp90wmq4npvml2vgeukklymlth4zc3et4cktnzdetyzatwa5r3p9rj2edtstd7c648pja9z5dgp3g7ehfwxeal87kfr8ndyqmr0ta2rmsmzfp7n8r0llsd5jgnk7ngvh5vr5kq26nw2dp8r37l7nuc98v2llgz8eshzalndkjuzxuh32tx4w2pn2vg5xydurh8d87ud9zryfd50jqywvff7pmt7p63d3qtyx9j9zz573sttkyh6p5v7lypf4lvxmpuf55syhn6l669qdwszkll9h3hj58dka4v378hahqxrg7gdpzrnm8gy4wav5gmvx83k8wvnvkhr7h70y79j0h46xxunxfqumkfeylhretm4t5pzxnp205mr2r3ltvplqvnpgmljfnnetfkwphj4g2t4hk6z9lcclf48cy7xa58hk24z2t3f7l5ll4m6yhrnsklqm4h0l4vu5qe0hzpte3nvv2rm9g8t34y7k9qdmgjsczu745ez6e2e8zwzjdvdtr652a6cq3q2qy267vf6hedsmuntf036qg0mcmnus4zgwv0r6gy4n7f7f9dnmjmcwe6s8yha2y4cnfp4t7g3dp3fta5x0ynluu26p7kcf985udmunpwysnp40slnp955z2anqlejmt0l5y4a4u5ueyduz47ar9dvta8nvw8g8tn0ngeurcu522deawk97c4w84h0hm6yr2gc69cp0pcsur9xhekcphw35xe2e9knqgz2m0dvvq3q5pztc7dufkxn2ej56awzz408m6frhfpx8vwwz9xy57lltsnz0tyc0dtney8y7td0hyjpjnvd4j6thcnalmk3ml0pdh2fjqrlmjyt0tx6udqpjxug92jee7wc8nw55m8wjcjgu5dsejhh9rz6utxremem97xklqnsg5rz7phyfzxeguax04e0mc7kf0eys326upy3mx694nqzt9hdmftrht3meh2td9qjrp85ersm3e34ccgugtflep0jpl4ctxv5q27ev778uydxuvjd7cxl9yxvu84gsrz8xz9qatj4steqzse2np7s6qprr379vvgxrppy9lhz9pfnkvur5y6lx4d2p0e02zu2uak0eklcsv69jcmp2tfkqwdp9wwl9zsduhsanwa20g092g9cay4jrz0ul40lcygtlquw7lmhqc3qawdyyrgs9zzc8wl59tx549wnlusfc46avmsw743s4vqckr8jx7h3algpjl69lkmv5udmpqnaxjxhf8sltxjfzt8w2763dratpz7w0vgcye0mtyv052s5k8829q3nzsxd9hly7xtr5evrgp9njgatjkkn96ehsq798u9fdg59vfzxt4yjyyveetq26zac52dt4962yqraqd8njgecyq2as7p3j567dmxf7dxh6lywyzjszawwxdjuhlqj59667tptwhnumy2nsn0vchsf9th8vap4nl6gy60lu2qmrtwgur307rce8wsqm442ahg95sh3n92jj7tcjhqyjn0j327rvjad0wsxlujvga9p8xaupdu3ml9k6gmj063cxx9vk55w68a0ucjxmre0qc8hlnun7gvmssrx6j4sz2uvuqs993ay6vqqzdcqyvzfmye4af7nrw975vez4pjphm948deqkde7dzfuhvqjexy8n9d5xy58ppks5eu5e6xgnt0gwmeq7fvm8vpk3cl8jawvmguxkkry5wt6ed6wgs2rg4wwcjeadh3wawxy7vmsh6cytv683hckpux903frf37gfumevt797eum6kdk8gznxeajgrj0ge6kqntglhtftems3utfy0csqzvmat37zkp0j7qexn5md724c35axq4j4kaulyuamzzr4rdmsedha49z6w82h9fnn7mx9ascjn4lc32pcsgxwajh5lyllfkj4j5ee3n6vlvdufrtzmgjp4xh7cngr9wtesucv9ze5375gqc6v0yx3k9fsls9h33qlflf8tjsspppcglt7m546vd933zrql0mg358nv0q43r8w02de9xd3jkejm0tgeyghcgcumsfhjew7nhwsy5yk38wxfdernaep2drz0c8xj4d4xxgryxgp5pvdpg8wk3ncj5yczlz5tqf3sg2xe8v9njzdheamzmxsl3mq90g4uv93mrw047lycepc0smxxjx7gkqy39vpmy4lkqh534hnwh7jmp0ar9h0w5z3vs6jteztweftr473h5prurtfh64lr0q3xycjzy9kg8wmhq956xurdkswr9n05ne8z800jkmwfvkxd9tk2kp7vljqfxfws7dj3kl7wpgqfptg4zha4fhn4ll5vklug7cs5gcmgtu4vvd3dhd8vpst8xjnf8nuu", Network::Main).unwrap(), NativeCurrencyAmount::coins(1)),

            // Generation address (for testing), generated on 9c1c4438 using `neptune-cli next-receiving-address` (third invocation)
            (ReceivingAddress::from_bech32m("nolgam1dx64scz84qrkqtwclxrvmucuv72d2zzn6hq7hgcpxjll0dcdznepky7w7vureuakdpm0gmpukl4zu902cyj640r7pcg9346269f4953l8g83vdxl964ca9vl4kjnrhzx63v9m6hgkj4nd9rgdkldlfma9pljndd38sdfeehd39fr49w8pa87y0jsp7udwlden9rsl08hw2p0qztcr54tu0m3cu7la48jeskt5fnjelphr5pz65c5rpum707jxqcglpzsj4lqwlwntqc587mkfc82f56vmh3p68yje7uwclkmmyr5cx4tg0dl9fp7naamkruxn0vfyh200gxpfgsszxsn27l7dt5ddnmh0pgz8rw7fl6zv72y48vtpyx0fezpms5w782gnl8te0nexvvdc08ttch2nf9swj2ln884ugncaj8x02plfvzwt5czntgcxu045xzzrph3tzt6aypz83e0mfdqknf2p44m8nvk3dkv0nm7upvk5m95jxj7nlr046ttvpp489wnqz0sspu863x2juu3dnt24jg86h2y6z63p5qme8fk235trhyvu2sg7rjx0x8732mp29ky3tm54l5ug3lnawrs3pj4hf0p82hk7yljq46mz4v9z67nzzymz4qxn67tpclg60y8a7ln38jlayckk924mpfemrqfderyn8pgduc8a0sy4z3c6yumry0x62jz3t6zh8euvwwpu92jg0w6jful7ydruz0hg5l46mz56524zh4q3xge74scpmd5ga08upyntv0ekftg2s3pgp35c4dke5rcyk259m82j6n9z3l35etwly2xlxjfkzglcy93xsdw5hqn4ajynpvuafute0s8es836ja6ldnwsc2a7u333nd2907wulg35j0d02u5a0cv2cffdsyjxvykps3lep3xsn7r8h9hcm0g9t4tqd9vavktw3ksq55eq9fu2alfdlh3udsqtdt0ex2kvqq0t2zwqwhmmxp78s7utl683vkffxalyngd7va85hsquqrdemwz29y3z2yua54p0djqqu8kwx3j5et4dpx7xq5qt4chrjqaknlfy4fccjc8c37eljchz5qng8lfz3gl8em44p2mzyvwh46ymmmy4753sqqnfht3l05kqh0vctp0lc2x4zus75rpy6t6c7gxddtzjx0xc6xhe0qvess3cjhmaaprkmk9nctk4za02mfg35ez7qu4ldl36xl77fwhdyyfe2snzgw049zc6j7m4xpldmn8ytkx80y6w0chg3p2477eyuatv0r6d5euf86hqmrx7acszl4s3xl696s707upm88s2nnp4tehxuenukt4lrktgyu5fhtzts7ygauwzwz0fz8tnah7vfymgzxf5lp9zynm04g53kmwjpvhu0nv79l30x6yn7xx98r6vgzchgff2gcw9gpgkfxgjz98xjy7samt43l05kn3hwzredm26wmgcp4ct3s363e3cvhma9zhjfwr4mr5sdvws0243teu70fuhpqp4w9snkvz2qenfswx2e2yammmdpw9jgdjkvs6lpxh6uhymjajsvhh7g2c7zk5rhxsccv9guppqd03tu0xa2u60s4zknumg44cxspqvm34gzzltm4ljcymyj98ewlf6a7g5s7hj8l46x640ng9q0cs4rv3vu5l7k5xcz5zyp679eqfsphzg86zpn55jzq2xw367yyuu4vecczhpd8qvmkgc2ygceydlrhq9rreup443qc4r0m5ejmu70jm78t5j4ncgw7sj8nz9pxyv27rjpdtpmqf8q72s6fmn4k9a38scgmz6ugskpeac7fnxxkcq0v5yqjsc9npe3c3n2nrxzvvlu6l9y4m65jueunuh5xvunp5vhc0lmzml5z3nr3ff57nyt6cn5ltzga66q4yt8nzvy9fu54rqajq82wcpulnmq666d7qaf8ceql65umz60838ezvczwcsw2qjtct3qtvkpvzukeflrtj820w48jvzvd9qu30tp3auwv3dt0xuw3we0vy39jc2aqanu5f6v3p4fz2c8clldgz6mrxktwrlu70gwk2h4pmevurr7gnj3a9avn9zf0mqgf3xjjk7vmmrkn9yck8wem88fffcjgqwh3pprwqg5q9j79ryj3dl6d56dundgu3vv7qqnysxdxlwu2fxnhz78w0g4knmxcpe4p97dxf4ye3kly6rw5lr49qgwkxy2qlyvrlzy080ekuqj3cmtmde4ft68k6j285wwmcrvt3xhlqkg2fndcqj7mr2ezae9vs8wp9attqmv2vq9xeyk6q2pfkq7s0hcat32tlru3g5jja5ntkwsspcejvzv6dpuup9atezshyxk7gsmex3rm4l3ycx02ytd3nmhrutpphgpjqvtnurgswxaqynrsgv6g9r0dslls7k9shlh0tfv5rzpy7j6mc3cxfl9w7sxlcegf7atcl4gezvqm9ddxxlzufe3rjxhme24vl4n2c9mr9t6s67m5vpvuhvtxfeq98wa58qku4vs7zfh8n09t2a5dygyrr0qfcyhejqnajt7d75dlnr25tav6hslx6wh8ylzyng7y50hyut4gf382jgk3xyly9w7rzs44q903dr7ux0cmqcy7hqm06cj07zuq20vnjrvkxcj40xzlr4y3vzp5kmvqhm0s5zsz9skpyyj0lkf06zfxr50z9me30j4w70vhurtjkgqcdkh6f0wlmsp238988mkkknuhg3xk5fs6pmxcdyh6lkjp62dtqcmxe2g3tcceqxaaprd2tfdycqnknavrlsxsmplp7g0nwkpp9j3vztzd0yx6jlegwuswrrdjz86879ykz8k2g3yl6790h0ccfpecef229yx8srn0aq65lqn5vyl35g73t5vq6242gscasrv2avz8kj9sklgd26s7emtacnlwkedy33n666dzyfh9xu6qjm3w93clmam4f9avqmtq9f3qzptq4gr5wcdec6222hvzxr4wp0ta9kucxnxwefet6rskzep2sfrwwg3zgwz9cx3d3rxpa3hs7pwfvjj2w5kj4tfhpunfpc4cvyzr964p8klwtkxc32mamrwa78u844js89v6666udf87vz2ql5300q4avkpqmx5v3rxd8fuk752gvs5gmh4s0p5mjpzu6wdjx299sydxwcdx4amzfnkjfaz22x8k25qu894r3zl2gk5d4futyv0zakd73vhedl4t3d92k24nuh3gu8jcvykux0nsuupd89pdd07sjk7xjyy825jh00xvntct9mxj3q7rhfagewl36havdk03lkzjq8v976as2tmh54va38g8e9mxvesdwduwya4nqa6ghx42x6nmq72mnnqjrs40yasydt8l682e4j4hyuwcfgyaxt7qvfsk7pp25rm8ydzyd60a2q9mw0hr02hq0g5tr08mkcfksk5k", Network::Main).unwrap(), NativeCurrencyAmount::coins(1)),

            // Actual premine recipients, added 2024-11-18, in 1981345bd86fcb8d14966ff4c546b117cf314a07
            (ReceivingAddress::from_bech32m("nolgam1v0j838fvl7ud8q964x7urcyetufmy9nfllmy0xej9837lanjl2vfmsar7w2ncraxhgpxg4d4pntd6kmjxhgvhjcdelr75zj4fqvws88dfunsn6fj0fnkxlch3rqcdstmpv28v732tvcvvctcjxw3drn5gpygmflhasj5zu043vzr6hmjpt4tedn94p4mzerp69xp9d8cx9u697f8ds4vrw7rmpk74lk06pt4q0lx4d5e7el5r0fywqxk3s8dvgxtehlvpde9kkt7y654ddpusmn0yhr4whgvh4nvffp2eahnvmslhruwymzpqdf5c99l9redvtuk9kg32zfenwjqkere2xltvfhqfpm54cxltcj8lfr5ttcakytztwcr457jyx6yuam6dma7khjkspx0nu0m5kut2neja956hkj3p3ej8hutavv2zkmygff4gca55hd6apyhhslsyfs24s60ldzktd7hj7gkdtgfm2l8vdd8pkuh02q0k3ce6utpltzpw6anx8zk8jn3f8kuansk3fs98zlxxpvqrl9w2ayq9axuc64lfvp9teluwhvwc7w2veujpqk3jxdag4d9uav7tt9lc02pnejw8v2fvl03a50jra8mf82j879z3wklw8n357fmtaaf48e8xaak7jfz6a6lmele0h8yhv6f9kf79ashn3sggh35kfqpx8wevnku7kz2kdrj4x5m3y4wze47g3rp3xvjpmlqx6yl7plxg8a33ljm0c09p2v5t36j2ym7t5u2fpcjvvy92vj3dnew8729u9lslrzuxa5yrmf3qntdegeq3hygru2uee5xya3mam2aejf285eh843vxkdvg8xeqg9lfyjkdgamchweg40srfju6aef0v3ads4wswyw6fyrnz8ltdhnd7cn0t7ucpwtdhfy8jcw94fygks8pxcuuerk0smkwrr6wvqegtdprtfzpya8c8gx9n8weaxjtfag7h7vqw957ncwkmfj27j7jvjewcapfef43ujgu4clyj8jhzkuyrlvgxz3vf2wdx4atl7l89q9qhwvtltqne4a0t64zwpx87aqkcvjysnuf6ct9h5uxyp3768fglzc6arvs00f3lznppgnuakzy0k7aw5620eddfu7eq0gusc7c348ng3yl85hm42y54leyshyfh4lhadzj927wl3sw4l5k00l4cwamfjxmaklp4e7qny3jvjvzkzs33c346prn4reljyn2rjpurms9ha2jcj55dfpwa33cj73nananj5mvy6s2w8ec0sftwzu2w0w3smjv3mvvf3glclwswvzrghy6jd7a2vtdr4dtrrw2tdfssvpa6qc2955f27rgrqt2yg2h3xs5qy94xtsz6ufjqtafl7cffhja6e0wznf8qnt9wd6ng062x9pgztrh5pvhqjcud9nqdsmydumz9h6lzneawln3ds4y8zfvjcnq4h654ypnts7wfwty9tr65nnmjvprqqrss9js325p94kh5yctshlkhuqt4705r5etgzavazw0w6l9yra56z9kxzwjxfcar407tfaw4l0elk3ddr54c4e8uqnnlg8gwuzsd84dmefvk958hmzdxvg2x5r995ez6y4wkqn08gyrwejf9z3unjtx097zehntjlklsfmfk2gw4v0eh9jd6jr0ztz8m22t7ajse3w8yyl6ht34hp8pdzr0qa0vvqyx4ulft6enutr3axjh9kdrcjzh28ky00vxkln7nmqp8xe9hvyqt2s2qgp8g5lu6tkmzav9vy7nq0ww5vp2x56gf6y9he6nm7f4xzqarwy52nan8m479h6drvmdnaxw5scddgwv669gpjwg7zd352tagdgwxz5gl9pvdcztzjur64jkg0hed60vws3al2dt3az4gfn00txe4c4s8djvzmml06degdccsuxnmxh3vrphjqeekqrgszu8w9jwksfrayeyhhlzxtzejgxy4c2apv4rknp0dq7rf6us2ns6zlkwpmul70fj3xv30uv9edcn87ggy55trph5plhzvf0vp0pz5yvq8p9hgrq3pl75wnvv074v9uldnn5tej0aewedvulv499mamxu88g3z4spactyzhl8jtgrypquasfvv2ucucg90lslzvsnxg5q2wgwc9c30ru6jf5ktmjfa3jm4whead4ln0l49y6z9l3p0eu4ax4qpns3kfuc5a6er5lltf65pxe46yaxcwf893wdqtaj8uz67t50nkwztwmp427auy65ck7506qf9ullc23vqy74sj2lnpgq84vtfyz74ywxza5kxzs4a3xtkvgg7a07u08dnlwqkkhkx6jj2srvsxk49nkutd3cd526n62fe6a9acmpah4l3uapzaja2y9cx3ncdmkpry7e869svyz7m0scm6vsyj9amxpctswkdft0tqp68fgyheluuy95x07m64pe45avnjcchtgulef7yrjqzmauhpx2j8xe9d529kx5j8wnpsnwyycduz59588hn75zzg09skrpjhe8mxj3ljds6k3p563n2n2v7w3d86amphe2fvu0ay25qcfqq39znjk34l85pxnv3q7ftl4x2cmkkr7gr3tz3jn72jkcj4xa92lzu0pp58mrp06gr8dual30y0r9pn5aeasggymtmswgpeyxqur8yqck75pfykz8lzzavwgv0fg43uly70tt4hs5e64hvn0t88w3j0wq3p34scwc5kdl79yj0xyz8zlelhr0yzn4fnwgs6c0te55v22eef7kzc32m8edu8rs5wymk6r9whnwkhy9lh0r5z8xssw7tel9nvsf7fjs8f9pjuxwe8s5nw7xhgh5s4rpfy3jg0kg6asjxrjcjts30436eud4ctrenesptz2x7ngv52h74dy7gaymvujxnxz7xmsl69v24lwj5580ndtrl5uqavt4v6nr85gh7ectvq0mpsyrcy3qaq6pwgkjy62uypwshegzrllcsre9wkau4v4ttv385qzgxt773pqxefs2nyd0wnpd5fk35uewa7gzghn5vwey07rlea2v5amd90njt95zm7umrdsp7l5q92gywjfuzyhmzup75pjxm067eym43vkxp03dcsv0tjsr253yz9kfpe3uw9j9v8tnlhhnrjgywzd88sqagsrdasm6prxzwf4qt4vp7r5f2vzswzdk94gk0mu5gzhnr4se47wykyyvs6ugvc8gg6n3sv5f4vvht0m2r9av296cmf2jdfs52jw8r05k3wqmvy7xn8frz0x2vc6wa66cmll68k9kxh9hj8dv59x96d020fgt004wwxpgquwl8mgsxm9tpcdqr55mce3ep3ckhmvjqzyzplm69xza2nrfjjcc82gp6y72s6e0cmwvw2x4qhjfp32l8jnr7njckl2yacscqffkmmtud7e2ttqrppte6v79rm0chzqpyv", Network::Main).unwrap(), NativeCurrencyAmount::coins(2000)),
            (ReceivingAddress::from_bech32m("nolgam1u9r0el5lvaz4883zzf588t0rgcm87anaxvgqq5e3ptzzzxh6da546lf2vhtwkfm29thvz6m94u6twxvzvve9m2ucda53fd8z80xt29wplnuq40u5r4pzw9k7eyexps2q3hsdqu5hhzpnpq2wmpq4lcfj4deqh09tdgmkuvw5y7ay2xqaaj0xc0aclnwd7qs7asclsvgr9r225alnfyf9x09xutyp629s2mfts2un2cmq9s07xwllw8cvsufulxmacruzfyxx9w0jkksys39w47vvgehlv6ncuzhl7tzrvj79xla9rfy20f2da5nptw9h5h6ua8dsn8q0qz0d7y5ndlcqz3whr7h40mjxgg3ueycs7zz9r0rc3znu8vph74nw22y38h89zy20lv82mekq3xu9cumytxqlsnugjs5tp78mgm3prpcmtzezgwzl8vntm00n3x99hhdvsp8lkjuvnjndxyczmgl554xl96x55djrjnv4mxvqlswzvxnw8lkw87f959v2pktmeg3yx86ku23fsfp0x8jh2te94vuk9ahwp7zkaw4njjsjp6a79qvxfcujwr89xhwgzzfjffnj5vusye7907s6vzgfms6xfzu07ugwr3dskakjazre2mn4xvxpzznjaatnztfla69c43x7un4er2c8x06ucwe72gzt9xvh487q6l8dq2ejwamfpllw5mrarrrvf9wu36r8pvwa45ck8005060lk67y74f09dz2h780ddruq79a5racn05gepc2qr0s85n8550ulvcmqzv8nmwxetaj7hl78mg8nx9ftju3ak2uf5ur45s26yle02s5netsdaukx7thfkalnr83f8kp7grajgx4glcknxzkc7326rttl34up9h7c7f7k76n43zgwjg8mwx22cyf0mc0g67kcf3637urst5fl2ez9ja2at7nhr00hug999e7g7e02nu7jxesq07zqdmpwta64zzxfvpwqy4lfd35qmyvm4ftjfdnw8mvglfmsflan5vz466svsq298pm748kc22cvyn35xfy6rzzq73dl8cytj7kchcx6mjj58ll2xdhk2kf0a25ea5quktkep8t9awrww6x6ynnepc8chgnvvtg90ueu7tgr0t25pnurcn6csukvpyvn988ynpmry2lrd8e6q6ur4k9nd3rd4xsrge6nlu5hcjrv2xhv5vmcavts8rapxrr4773u0dthe08pd3yu2vnetkk28k08ae6t3dchr2hlfq6tlmnq20taj6xz7uq7wccx30ntrf9h7r2r5tgmpd6c7m3vgq69qtft3xlw8qs8tc00j4vslhudmv2cgd0mgncgvzrmvetrnt0zmwft9c6xkeyt609cmxc405jzwt3ysj249vmhh7heq2j7524qthgsu3d0ns9rc9amslk2anscy67rrn456pe5kjcrr9ft9javak5gl49qtyegnyuh6r7jgsfv3w6v8zl6zy2qww7v7e3yyvdmwvfhfusuj9kvacrtwpa55fm283ez4jyz5t59esyj4df0qnly0vn7j4h03zyhquw6psxcu8zwz3edylauq922j6ggrztsyemaj6wm6887saw329kg6gpa78zp89z5vwthawkagh9tf5usjsdqqdlj3v0047ecwshruz2u9mr2fnnc8m0as0qj7unpvyhlyem3ud4refpwx233hg2gtnj28duea985zmr4hmq6dzq03sp7jglgvx427s75u8czzxmsj9uyasq4qke6nanxmr67xekf60p3wgakd339ry2z5nkrey5qwtewknrvzaelnzl0nfwzlvkaws3mgng8dzy6nx5y097algffhh7ln7qt9uetuz6jehtctc9q6jevdvee2g73lsqxmel8decsqephv3cdtrly0q63ztrjx7hr0tudfuhy402zjd9yg8227x2ve2eda8qw9hcw5lv7ec77t77fqdjpu3stn6rzlvavupd6vxdf4nkm743xtdu2c5kuqhl8z449njj3vqu8q3x3t60lc2cppedllv2wy6xdfjlnk7mjuupesl5z67f7qcjr349wj4e9zg5fxy7qax2j4sgef86khhvnhjec9u0v79n29ah79eynj5utvksvuwp4zreceq6fke7x0pcte8s2506u3kzd46ps06znwydz9z727vsdvalnxlj4v7rdeqnrvw8ckl9pjt8fpv0773lydy3n6te63s37729j897d2emfyk0f45xmjgzdrsmtvpf6ejc0mljque390su0lf8yvd3lsducr6cvnagkkketfxxha45rvh9dw84wpl3x8skzjh54ht2u7ljv8mxww67lrfgq6lmx7vx0qdfwufz33mpuychkpz2hr00ptsshuukp4yrn95v2qg0ldumvx07lz8864kq4rwmhedcd8g6f7sqyewqqxz38drudukyytany97t3m7sp2zucntlqpk860kggcf0nydppvw46gyrg2f8rwq4f03y5w560mqgf4eaumkq74qzzh332p724pap7lhqv2tru2562aadlvyfaqj433zmqt2qjerz6c6p8aqq55wm98xsjp8d6vzsvuj07k5tlqqjdjanwecq9z3k86dcjc38hdgkurjnhmtux5nr3u84cjkzp5qxv6k86z4jvl73zxcc66uk2278jw5tvkr36j7pqnkdy9xkhdnycu8vlp4sne084e9r37n34xt238a95ntws0knuxlx5xkw3g3p7x8qvn2a9xey5yxk9zrupr4tf42lc7qx0thwh4mga63ucgkwffkc06kw58rh5zx2vf4yhdpx72lq9nwyvg0vkuhpjyswfuulesdldsrym0jm66zh4qyw96zc26xnk9ltd3zlpyj8ql9r3wq8sn8ww9w7andwx0kgqa3d69mnkz5mvydqls4zsm8npk8s75fgf4h3n666sghn72ryfn9xpdvyqk847g7w68mxu6rq7kvfrwsq4y7a3gt84rfk2cfkdvrraxecuzp3lmr6kde8gz92f2khrhynwe4dkjwrs9lhs9xkpew0jl6drfh945h9ejev8l8s4scrnc5qu4g94hvpswk7a69x6wmz3hv3h6s6udz2akjdpq2gmm0zq9r7llprzan7ls3l3wpcmjh9dlzg0e9tjvqngvzptksl9vzjrnfhperw5xkvaaaum50062a5wpz7uhrjxesrxa97r4vpv5hmuywrtwwlpzavgfacql6s6d5upvy8506ma8kddtmak0hwm55cxj0xw7ntukq20x076krwkcsp94hrq9k5uxp9ppdeyzdju7r9alushp0q0pw47lj99ah74g9y7d67ndw266xwlkar5zukztf6ffzcaza2p23nlqpgcsv7fpgvu86029wlfv6ke9hvcwqkw3ddn0rul6h9trcs04nnvnzukzfpf4nkz87dqw2", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1jv5eueyvcq0eyej64h20j2k56r33rev830gmdxe038kwqjrcr3n889wu7at7njguhfkzahe6fc7jzpjrv5v6q4mak0hmtwsuj2g272e9yjshvwqlhuj4tsfhy9kkepa3068lxye9ta5l48zck5wkx5zj2ww3zfr3gk7pfrx2mv8zc2gdx2qwh43wjv2ztpcs5w0kfkalt950eupzx3jd0xu56aaumeg5try2vfdy49l96pzsetre9zt97skm6dqrckm8543suas4udnlh0l8a98rdxyljuhyk4zg29pculzdjr484xfeg9dfcf4l2ans3rkglug2hd3khczkwlgejv4wu8rxxuxcmg38f2qyn7rnjm278npfeh0fzn2q8me9tmxx4vg9tn9j36zv522fdg4uqvncn635v9k6hegwf48xffh5r3tdz0cm4hz2tatamfw4qtsc8mhnnsvwq4zv795kvv3xvms92w22trar6c2xsxsk9cvlpnq0aje7gq90h979v5vlt6u240yhck5mlphu3tftyy27vrh79y7crx42yus5y78dsml3w0wd3k68ykk2f6469yrl8mdf6c4d5pp7qav2cahp2gaf8qedggvdqx630yea3zvkrxs99p0ahys8pwr9xrzvjl9uq49rtnwtx78ad0scpsvp0ty3y2vu7zqjq6jrwphvndkkq9al0phgc36vjj5mlzcpxj87yp94x0ysuupmmc3z054wqy5xn028hwlt37yuh8f05m95j4xeju7c5d874qguw4g3v9nkcfyusrqhj3mk4fjj84zl4ttpr02ncj49rqjf8vydtyf4d2mtw0jkr68avrs5zama43v53xw0nvy0rag9mes3f7hw7ye2hfx08jjgjwk8q6zwc653jnvzcznenc5lqw456ww0s0ffplwwqfp75c64kvu4tcc4jzmg8fcqt896zaa5tv4hpwfgnw8yx9564g4u2j43er55znwez22s8qnqa2aqxfph99qvlrzs8pmrw95x5pnf4vk56y7f9dhu33xkczf449qmtr8fttnwg5pqy7a2wuz0x5n7r6f4gnytpyuhr6xfx3q07p8ze9gl906y2nlyhjad2kx7knzcxr73texmc8v2fd225duywrunp329qr9r93xxfs80sk94qknnxgd0grqx6mssdrsayl7ygkvehw23fhg66zzldv3njyhdyrfpmxctnm5k2l4kaj5wt35xsk3cn0yz5pd8jx045kp3akks6pha0qtxzaevze88mar7umrhqxfq26sfxmk3hzw3s66yaz8gdeck4krc7kjc0vt0fvltf4msyhehwep6clwnfyxrsgwnqxxxdydm0s9j3ezeuvj267ptmnge8zvskn3ceuut957en6zyvkerrdcgs6a3g7hft4kppaqkw3ut4rl5t8reqkpqwluy7haays630hzgwuwrgqly5emapcswdy3v6tneyjc8qsd642xp0hue8dfecmd8cjh8e0ua04tmwg4n52xx4d49gw7rhq2djg2x976kyn0ukg8s2h09k7swglgw8mf9mp3dccrsfrynkrqgt2tnsr4e5k5pdet6nllmersnmcy6xd9szkfdtfyvjplq8edkvjts6zpa5l92ew6ycfwrk7c7ful4madmjtvhrjnhqcvhvczhmgp07hm74g2jfpvtrca38pvxnfllyvs4fk5eg3m426fsepfkp7d4283zz2j285vpgkxn4tgwk2wvt7wvuxc5capvmttt2x0hm78aksm7f4gzmvc5s477jhzmn5843dd43zef9xap7fyfcdww308sjdj0zryarpsltxnqzx04mcu0avdsxrnsnwly7yqf007nkelzd0twcjmytddncxr2znyhvw4d67ftkt8k48w2x8a0h5hscxew2l7z04v7duq6nlyfkczhne6hnp2ayrjrvvyepdvnwaunayl0fr4w42dkmqnj303ddu2jjrnf3p0qh4wdenx9uega42yte3cwsje6sadh3uxf25dmcpu4jh6zwrh754saa7qf52t8vrfkh54gdadnsk3yfsty3lwa5j0jl804ecgtn4j4z5rglujfzyk4ckc2q6fn37ldwtr45ys8ekc2ryy6r44fkym2npj6ma38wmxsx2ug02nmnkt6ur9rd0mrhc4qzdxutdn7wyxyfkvc9mz600qeqwt09xyvpnz5j2wnnv6wxuava5h0g2sfg38wlq8k77dqkp9xuyd09a69h2rujrw0pdq4qw26jkvjwkvp7lu9aqnhmus87pq0yt36yywlg527yq465cmfuxrl397gt7gaj4z97lfkmfz5kqlurgs6jn93rpgw7ymc7du23facsk6q5txaeez5639ewqhld8zrjwy29ld4hksmw6fpk7kr0da6w3q6wt8gdte2tl9kp3guevzh6rzd22zx7mzkgyr4u0zzq5zdyn27wl40ht5e6zpggg0r7exjh68heazhkye9jd5ant25a8hg5v3xlpmnjwzv35fw44jt76edtfa9sh0g6ylv3s3lx9ap5xea5xcxc8w5t6jnyaf8qmq73gt57v4nrhswy5t5fgp96kstc3xq49wy9fxak30ehkscqr59fxmsweqvyk4adhhqx6l73xzgv2wj6gjfl3lsqgtgcrwq3f4cm4ctzuh9adguvu0vmvc39ur42xktnkqulsmwygjpkn9e42at4phc3nn0v0cc7qya8c0053kxgqj7tdcyr09z6nv22qv309vgv9dds85hhep2mq2y95c7tvjcur0l0thz29w5vzd64afvessaw0nqaer09qmcplvgdy408me4e4kfn6zn509pyf8cetzs5n4y42wl2es0qzyjtf4fwsrjvt7el7grt8hhk3ls2pymaasrsp6mkqcmyzk9j023k8wxdkl4g6anhlwrfaenjxrt4j97g8ds0gg233zct6ewwrr6rmc5us74ypf6xpu2meg6674jk7wujrrrs8mk2n5n7vnz39dq4094uztxmvrau6pmv3w70uqhncph5hgwpfq53auhcz74cnh8wzgappnaayret7xf9dfud7gh35tesmv5z8u7e0gdmjpar35wfk6ml6ldzwnwtq7kl39c8p40txnmcyk0uhsc8hp2yhmqsghjfh5n9paz76a72a9j3nwughm7xpuvfzzry94dwfvwrdz2fkq5u9xm6v486p35mpx606qpwzp4rs5mw9rpcaa5qdt5wuggcjgc5ztg2v5c9keq3e7a7njgzxcay8ykxxg5lnsx5548fhl5etvt6vz397mrkj97s5f7xa7tg222heqjzxwlqg7fwee900znu0l4nm8jf5hqdv37j9hmrw0vj972le397qp39wphqpjfur6fl6sct037pt5hfm56ngrlgu0e6q4ggz4mrv9r0l3nyjx70dx8z542y9c", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),

            // Actual premine recipients, added 2025-01-13, in 88db6fefa150fb13cab35bee3cb5821cc6f0eded
            (ReceivingAddress::from_bech32m("nolgam125sf39wwkzc7pakked34mv74v67p6a5p8nst0st2gmgf2tsr5lejzmnrp7vp3yltjhv4wj39cw7w2zj94zc77rkdxmchrf5w7l588saeecwxtswqj4n5z22af2f4k5ls37vcpdlc34qelvqyw4nu4cztchjppc3guc30yssn0h92jnrmmqak47fgwg40yz4cezd4z38khc4km7hzw6sndfd4s7l75adu9zvvyydp6zww7h6geqspp6qhyk3h3qgmgcl4fqy6n2j85tyqwg97207zxcywpeqmrw0qq3agumx9gyuy32j880vr3x9x0ww0dvngfhdynrtlz6r5afgyycm65yrthfkn6cq48u6s4w2waslln3cynylvw8qt30n2z70dqlcht5f39dsyyh00sguhnxk0wu3dwevc2rg3q5g3fwcrlxqd9dvqxfaypkvvuw68uzm0hp2q4wa3vkfkjm85060ppax6ujf2xnulejfv2ndl66fazpvuayv5ltksdz4xzpxs6uszx5chghg3hnllm9u8vsre9zlndkcyeu44mnyutkq9tag6sq9hpd9r88ysjkccy5ch9hczrq783wa8hns0cwegvdadm30aszwxzcjfp3recp0s7xx4zvexewmqfn0qwc3u7q5qcw6xv7ac9fenfqh6j2p26p435skpzk52eyl7hf2p48dj56t9gkk7n894mcvxpk3enqduk3hm53u559dcg5823xa0ewpktdek8c3vfn48fkes50n9mf2t9k6jwts45da068acyhuw0rehanu932gem963kj0nsk0ux07k0xwzhx7ey93vchr33hc4tdwht48aly4wnf8elqeymtp463xxalruq9f4q5drfpkkwu325q7cxnvm5y5fcjy8sweq7ayfggefzp3j22ylm2k68zymp7ymwj7wp6alrzgnm4gjsppx8anu3yeapxraf9t4j39afuknu2r4h7h6d2zgunckfvd59e2522lx3qz0ffaswmznryruduwd73nqrg4re7lmcfaakz6wn2z9c6vlahhkeu4p4hejk2jh2zq2jjl4rn0y78cgcckujeenlad7zdsl2dy9972qxqmpuw8lemaqq3f97sze575m5x3vjzvctnwtkpvm9lj453337xkqul0g4umv5hdl6umy8sser076t4kda7ym5yppmnazya2ycp0dhpg07p9ylmrzf635awzdpth094wac6k7p6u9r52kstym5vhymdfk588jzzqss2e0du60k2r3qn76j66fpgkge2a4kvhd0t9uan477xha88gxt0l9cjlntzcnjtln7nxst7a6hd8vd4mg4ggd6faqqftc49t3hcvwgj2cgph72ak3wlklv2seda4exzhscm32mn8v2cqv6rpkxykhy3rm57jd9ggqa6h8hlx4nufh7n9xlx2q2w7hcw9f387pkrsqnera4dwqv9wv7a0lcdp52uw3z764y8z25msz3y6m35rxqcuyyt9e8203xm20k7f4x3dutgpjwjkr88l2rq80csy42m9wpqkc3vp2m40t0sux8p6haeze060tkcm58z9w6sr7npujsnsl3hu9mvtpdn5aaeydwujlwyy3u882ec7lkf0jecsmkhz9w3e6kc2h2fej4qcmxxddt4xwzawgnaflxmkv7qxq82aqxsyd6lgu6xz6s8q2wpp69fkc52as4d4hx7df8xqpjazn0j7j2eygfchsuexmht2zz9a3rzn40uny5mf5njlr0psj95uxe0086gt77cdnzm7vd5lxq36qs63xccg73ugudtz9np32yc3e585dsc9gfxzuujsj45gvw0f4s4ddpthmeq4v7hwkplh4ejareen8kz5xdu849z4uzkdgkpzwz0rnnvnu9zeck093pwpuh3rglx4lj0kfdwshys90n2x0c26u4tnkhuv74d4yvsr8n3ym7gegchcy9j9nlrkrfr58mq6fyrl90lkefa57u9ajajmxgu9el80m5kv9c7ne0u8c2uea4fdzckunvm8tarnc50u0t6xgg96nmc3h5a9l7acfszapj2fwcmge754992k4qsftf4z498splx2df0f2kzxjswtrk8dwnzhkvwhw345u86jffsdh7y58aklssglj3zsatdprph9jd6a8kt3w0f24m5elhyu3f6aqp5rtj5mzm4md5v88fgkl4a7lusplcyhj3kq5kq2y5ehklm9gsqscrd2nx27zma43gp403nd5wgzdclalme4ucnj7uschq73v6vhk8kvmn0remaeqxjh2pg4nqgfskd0ktjleuj573gxnkeczn26pkd86kw3qtll3djdl7rs3844dte764ms8aapmpjq76de6mcxvx9pv2xtyst4832vc2yzt6426rk7rff4tslrsunq7azyqrs59j3w6s23tdcs0fw7a2yh0s0f2rd38rzs0d5dawq5akmnqnd733dw0xjyhsqh320ecnpx68e8673d3n02axs49zwjv8t6p4ftajezqyedvpvvnm6el4c0s77pxacmgyg8capd5069djw3hvn09ugv5889tcla70yzyu95p9hxgcdw5ndv4mgr60wgqkvsquppzx5yj54sxessflvtr4vu8sk45xqkevz2jgj8r4dp5ye3wlelp7mlgjdrwls69z3nkztq70s7zjynd8x9fkxwtdjpzmhvyp5su5ekt96vngxau40zkjlp4m9lecsemw477sx4svrcjlq8932gl5fwgzspy5u9w3483f7ekuzwff72x0a589vtphn9lmdt0l3krj2pmujyvddqd7fd23zppm0az58kynt74h33r6y8atk0tua4gydh4sx0p4y72l2hrulpz3vdnvt8vklq5gtyyk05lxglyj743x869mc9gcjh6v452mpulgec3vg0kw32yrh826y9tzqadh7fsz4h7rmdl2uwuf4quu6q9xg9rz4f6v4e3h8dh8h3hyk9077fcdrgygy6tp32erqzhfmyws5w48ur346fhudztvn8428r5zc4fdmt07pwxp4mu39xf6er6g44eazy30z9ylt76amnp78494nejcups6yej0awzhdcrp078227ljcv4zgendhjknlzw9j6et2levyn88gaals93gkednyzn7dac5dkw3r3qvffe0kzgxvn8x90ulu02q980jfczyfu8xe7w9ffpq5gpcr6mprj705z8vy7ssl5k608k3z6njvtfgddmrx3kmxxz3puwfujrv9tcg98jxee0c9t9ga2nqt5yw2pv35a0ktpshmagg8srtn8phqajn4vnz3ueld8kg97tf0u526aljsnz7pgwmzd836zsr6d5k2hentxfspl0r6rpae736jhpykmg68j9j2e4xyzcee85g6jwa7m0wplgsl8e4qwtpx3j27mf9tlac9ghp2u", Network::Main).unwrap(), NativeCurrencyAmount::coins(8808)),
            (ReceivingAddress::from_bech32m("nolgam1xt6pchtatfkgr4g8gd6u5w2n2gzh4ntzm08ss30hu7dnjmsexeqs293lzel5cpatae8h5psjvhruh4vtrshuvqecg7x555qxt4k3hmpxgcunhgjacwt0glalxfsz0s62q6g3n0tm8rvcrz4pd7rx9t3rlzcv4czsft0hfnwq05z8k5305rpajp06m5hthxp96zcf567s9ujjjjhccga6yt0gm8syhsznj4twt5mchlqg0w3z6852k3956yypged7thekfvs536tvmrvvlqg0d9weu8ww0tcvs7hrnq9yh42638dl0nene3tea5wdqtknakygmcpy66uuyshes4zy0gdt3h0m2nx2f4dk4cad4hj93hfg2lhrwet934mghsrpxj60de5lr35gvlup9v9zl3xg9qxet0wly6arzguryf2mtwa7n2trjzdlleq06kna9z826xcxpf078j7450vd0cmr8yemve959qmru4vfegpcjdntzucr3ph8rd8a6hj04sdxu2smahgpguclyl7qxcklcupqugravk2qcswar0d0ycygrxyefwcvrs77s6md5gyc8acq4uanzjvplsaa2ukawlv4qrgrrsnhfld0f6zrwgns4fe56vzej26hp49ufs74tsmq5tg29357as2vgnd4hqe8g0glzh3vsed0sap3kprxnr3klmqm9yp3jzmzynq489qkep7q40d59y8khxt9ezhkmtgtmukj5tw7tnhjrnrcv6e7pmj3juyqu0kecel909tdruaw5c4mr8a5lvhu5vhauaynsz96nc3az0pwvf3h99grpxr64mxk5gw4ft2w2yxfcux9wc8td7kfxtx0x274yy7qkxetqwdsjlgemqkgp9vq05zmrg2h83rqma3qm77yhwaunpwndjsejcmaqznkvgl9grkr64d950lwfwwl8d9wk6lc2n8xdla6fklyes97pc3ql2nedhx4wuz28cwwy94vc06zu43tlwn3ducurvf6szcqknlgh55dx3umel4jr59py6exk6xs8j0mn4w8pjsjm36aqqd4n2jh6yevvdufzl4mn0kn00rgtkpgqydhzhrt40v02kuteyzjmrdrlzayjsjjm5yt9wtsrwfetv66rdvmjrh04962a3wy9ucl9y7swp5l4yxv9qnz5p0h9tghw9cglqxkth8306znjfh25u36z3fnpz83fg8905f265rjhvq9csscgv9gqfgqdgay2c6pqdaazvr2h0xntagdmtuvfpzs02san7wvyxtgqe35r39l4qykvpws43vlyrv7fyueqctapmsqnztx6s826tcv9smgx5khef02jjp6r9wkmflyskxnvvn3fkzx5rd6gx5gp7ef0hnfzhvu5w3wv8va423c7fr63tugvhzklaqzm9m4803fu6r2gks9gusp0638ye4zsfexl0t6fnepajmf6pjj6snc9s9en34l7n6tns4a9spehntfzrz0u55jlt95jz9gh9lwqwtakd2d8qqfvfq8fyugfnfq0xs9xkk7nz5gqugp6022vhjlrdkkaew9nhnl73ygrzsm9eevvqujwgzy0ukzw8e5mhsklahhmdx5gqmaa0twy9s0gvlmlcnsatv3jrk50j62x5x4rysd75aa6xt9jc6tjxahnjgskveywuxk86jh2mqqm53zn59c3mqu9tw60pdu3jkwm89mqxyve8fk85xfzfj5yzpwarel8z43y40jqgfxdukn48w2jy0ps2p7d9d2r6sl6jlqhmmrafgmlptf3pjfaw7qdqs9kprvjyv3j39a88s69vnzh48nwevmx92rjrg6xmck5tc5fayrl3khea6sx3qwnkdkflk7ejzwm3wqu7d2z2mxtk38qe8wg0982gdwaaj7td7wwa0vpqzqxy38mv0ddx6seefc8txl6u7hjz8laaf4ju78r6mr8zvtzezdqx73svtynpe3pkcqlsxe5q692dpp098s06hmgf390d3wel5r3tgud527cx5hwf0hh0yej6gs7j9dztlappmmusd4z9nqra6fa78qp9007k2q7wwlajaqgymzua8yuepkm7fag6dfuz7s276exj6g57l76wta85qytglch2akjny8f8athyagxhag9fceqmtl3rm25nm3mlvuawu4hgv53xwkya9s9nm7eqyt0h9p0f0492salddgs2zfw3ehqym7ddkpfmdzu2njptdpcgtcekz9nh98tjt34hlxvtdu49jr56cv33c5hacumald9jdlam6y7fh4f42afv70hrjyjumak6fhwpye8afk6d79j9nw9rd0pfcxjk4xeaz24p58pahs2fccvkzuuym5aamxmj2mtcp4mqrw2xycelrlrkl79xr2ylaqce3gma4zc6q6rq8843v25slx7l3ya8vclmnvwnu6vp7ldefstvek7ttplrjl3ezscjg3ynnesq4yjvax6vxfd0sdhaut0lmfefck4pwrvy53a96x7crg3fh68xesepx23qzn44d8wrjx3rzhv4sslmy4u3zu49u2m6wmcjvu3r9gredvgm2n56rsf7kxn389ftsz8g97jlnztwzlc8fcpht2apr8u0a3cz35x4zmevns0m8ph7ncrn9zjpxypkxmax8n5x3yl5h3ufek5vp68t84qmhs0udgcsx8kr3uktfhskd6sgwwdc6twzy8tldhflhahg5zgphe575fsslqdyfzwwcj5wuh7fx5393uezw4y9u5ch8xgzqy6we42xtktyk6lwzt6xnyhzemgg7c4c4n0kpgds2kgll7y2tlgkqslytlmt0ve64ze53e60ef3xzvf3wa6kn50kfksfjka7n4sptm8kf5e09azd7tnl7q3l5h3mey8zwzxx83226s38qf280yaglgx9fmdv0xrqjrexcqmp9n6aa8jqvqnlq9gxayxera7cyzcy3pjy8as53gxpfs40wsk6a03uyy2t9779zujgap6y3kje6vvncgdeutj8ke2rs8jfrltp7rl3e5snlq9phdy2ny7rn4k2qkalrt97zz2hvk7jhcj4st5hsh906qngm87vx3as85u2rfusfravtfm7yjvvwldty8uak09cjy5a2yp3v7akltgzv6j0y75x0g4zsq62e4crzzavn4x9s92wtngfyzckza492tkju8c53m0jtpm62yradzcjg3ny3kd3edp0zznneu9zh8h4wfn992y7s5l6vmvukcrt6gx4txhdmz4rksq4r2pwvpkfaywpapxyd82ajvj5cjtltdz7pa4xhkst6hyxfacpnw506gu5rn9kt9q2jk95exddgqcnzakfxfy66432y0r3ehtv72508dhksdy490uq87ekfve845a2jsret7svtu802kwzw4rgc0v5e8l9q45gqsn7ajk0ggfjcsp0k6fzf49ztc7q7pw0h", Network::Main).unwrap(), NativeCurrencyAmount::coins(384)),

            // Actual premine recipients, added 2025-02-05, in fd3f31a46186100d66e0bec0df9a8c7c886a9417
            (ReceivingAddress::from_bech32m("nolgam103rpk0ags3sklwl2k4pvpgtlzxpzty54mrqwr8qt224kc54h5w8m8pxggvj7xmtjkkhd2t8eln5v5akvsjc9a79fpw8hlyt2t8l0cg9003hjwpn7hfwu3pcp28w4maprwtthrum2vdzrzp7cz8f270uuvp4lm7u335rqmxaxay9qwj95mxw4yamymeqts0xp7uyhk2dk7ewu2kl5ld09c9egt62zk98lvx787fw37d2gg3vqa3tr3e6m5wa9ck96glmwdc9npv8x68js0nveqlprzdv660yhxkpy9rwpeqjg4l4h92jjtyp9vvkgdtft6ptfgxtv5reyfzqlcvc4utlmmj7fzrshwqz4fgvfe674v6rzsgxz3q3dz96jpgdcrxmnmu4ycp7g76vyjmdusjdgeczf6jkks3mfded6pumjkd43njvdfky6ss3d03kfl2l7ycyv5fhccvsth3ccc997jx8pmhsnhd9qtkx2r9v9atuq95gnckapfx9sepd8kmz3xnz5a4dchvwldmhss0al3evefc7mfxwpkhav09dff66yhzv2rdrzmdgnd20y0jpdzx7jhh7dgj9vqt52ym5q8wc24ddzz3wua4xh3tay2mtch4q6xlal5zaljuscvda7my70ql5hczl8jh4y8scgx7kh0xlcwrne5czrrjzhxgnh7vjavuut9danpllap48rjld7nnqetp4x0c0ey4m9lrqgu3uyyz7cxlwxdz46z7x4ad3vukwjsqdn0xjtahx0wfh3pw3jn299q3jaqdpa7l08evwynycdc5vz9tnzg9e7gt0gsz52vsl7j2knr2xwage742d595egg2qw9jyvfgvd9f6tdzk5ac3fsxax7yxm7qpme806f36lmltqndmf3de22sjuj97rr8efzsj6ucmmt9nfrywgva6z34aycrk4jkkduncehlzg83csjpk256l9u4cq98ygg3x9x5y5cq4vrxzxfa6khzf5ua4amlzx45g393y0cwfkl25mzs82fffe7m25455evcdpvhs4w6ml0wm57rwz57vvtd3dxrqc5tlz0w48extcpq9ldasmy4vedzphatu4w0qqzunewtx2hjm84h5e3jm0fpmfulrf6da3dmv2k0mrre8t38cmlk8ske4m5z7k5sv22nsnr2q0g39d76ns25vz2c06ak944jr50a75cc7mj2dum54vzvqts67hwadhgcgr32dewpxud3gknm4gfv5x3v2v7azsjez2p4de5ah6uux8gz58mcvqraf5xznjfzj9lyld06dp938a02lhdsg0pgzhkdjaylqwxq777al7gq5s73fmswjzzleaqdfrfltgecwwx0ly8kryxdgz5xjl3j6z74d8p02crcd7h0zzut6lcfy6t59cqy7uf66v7rpfvxmq53c2jfwmfy6y7cgk46x5xl4s7relm8ldgg4ag04es6zlnpps0gwdrdk88rzfpmg4t4fdst83xafdmf9elgrde9x8fmg2wwgzdd40mk9dhhsyezzskurvey9jhap6mynxp9nry7ed6qju90zz3dn3nzl95496h4ul70vsgud9txt5ya47vzpzddeyakhdm5w86c6zp589f52vp0ktg9s88er3dtf8ts8e0u5spdk0ay85s65qsjfdrfnqe3c93l6s997x9639jx6rha2v4nl6uzleqdy8w4v7d5ezd9m4csa42j36y09ulcj9x7hdvlp2g7dzgnel7kl9xf0auea08jl5yfxs0cca7ufcv2a7hd6wrtxpvqtpxrdhq5q8xql6rxqzdladn229skrds4xhucqc39wj6mkuvuac9pxqnp2kgxuyq5jrwvesewekp2g5hynrkz3c0q3xrheuk0692rdj35qdk7vg5m3lwnyn7ttq3ydat6z5z4w3cafwr6xden7vq623p3yue09xs9tewq0cqfazsqp439cau4a4qcu4ppkeuqg9g07kvmdrtg96ymdqfqkks3dl6aq68m3emrpvs2z0xnxts95sejumx6cy3ywmxm0nfa2pm7ynysl0cy2suu25a4e40zhrjdrjxskm6xzj48kh4catwsv9wprnjvwc5qrdd58wflcn5t5ymc7e5h609n7d0yyqdrl3d9cjus7f2cykpkpq8dcwp9urdxfvwt2n58xfq2y8vxm8rdvujsg25vzkw0gh3jgg0r5r4mef2yq43q95smq023ndt7ujunmpk9h8dvl6tg2kacurnt2fmkvgq2wc0zhw3tuvtfe74swx6prqzkljpuz95g3dhhv3u8ee073484r2sxkj5af46n0k4vcrf7uj08z54a906k5hx2u4z2kutljeer3dhd2g2t3m0tf4u5le9sh3rtrham4xn6ewd59cy6uns59lfeaa7zdhyzvks7jdnk7at5x9eq33fmdl3ly6aplkyt9wlvz58l67r2kw7pj8rhh7h79xdynd4t80gk6rlq5c32dhz7h9muq5wt4u5de8qrcwwdu9yx0hcuy0cnmzcgdzs4zkrknm4nvwpuzv7te9ytczn8tlnl902ftj203vxkc63gjls853f9wxntktv69lge3gz9sldrdfdj7nxkzcwxdv8453zgwyrxm9gjsrcm6nzkd8v3qkjnef43mqnky4p30pjm5a05xclavntfgr3wwafxh5rdkj3g4v5nl9e4xhu9zeruw6jgkxs6pudn04yz6gexss0urks4q0neayy44x6lj9ugp8gmgpe2jkpfpm3c7psadh8xpqg8whas78qzj248s0gjp9x8829rmesh282uvwhjsz9x43r8va5ferm3ful2us6cpj0mehvvpgnn6m3xyxaqzcvjx5w2zrmcch9dh5q7cavkd39a67npyygurp78rk9zaj3efpn63ky8rlxeqdqdej65kuww99c53zhsyen3q6w9ly7html32qlfl48u3gzalw7s3yztnflrhyr5gpzugyqm0u4meknkkdazre6c77qfup3udkwrvsy6qdx0xr4nr4harelltkt0vlngsncjz9fqtuj5s9rchkqg6mv4hrzrjg2fqvtanwsyhf9kw6rdk3dpcwtay7w3zrcf4340kr96g5typ3w53ujjg6374w6lc4pptmwawdl3lmhry9a3jc4hp4mkn7xaj0xy8ly6crrg27hu64l4r6k7altnmr6yvtd3zp9avfda453dedgf5qsa0m4xjfgt5skavzmk60jpmv99krxzkauh5k9nxzkqempm84ayu9dj86y4s9j23a5x08c6e90l9y964c8l80xq6ew4zlfa2cznlyks9vndyu448grzv9uc9kgfjvwhytud2mzcz05nlgwtgy8790uhctkkqk2h77nhu5fk9pltuf67xwxeu7jm3lfy5m0fj8utuwx3uxcuuq60jwtr3hqcgtd8tfp0t7ytuspqk", Network::Main).unwrap(), NativeCurrencyAmount::coins(441)),
            (ReceivingAddress::from_bech32m("nolgam1z5zqkpeknd55alv6thqnjvmuuylf3vhulp6nlxq4qtvdwgel8wyquq68essj0z4tqrylm5n2a5qqdgvmalus98rpnxj7jyujapk78qgdn7ydn2jz4fkny50fwcwpur6h8mpjp52jhsy6lsql2sjywqh66mh0tyq24rgdar8jy5k6uxpy2w0dclw2ldhu0jm3pkqegmektkx9tth38n0le3vdjmaa8vlr5dq0mv2tjvzx03vw8t6ecpqerzsxl8ekvjlku0yxmfm4cuh8dr4var0v6ask4wkwfgjrkjhyyc205yc4ek8ezdkzyust4j69hfldlrfxej0h0356a6j7lma3l9kwc25zxvrur9p3wug48zdp7l9cnlakh67z29vawtend5xyan2yp34krh9mwswefefelvqjuugcrrq0stgcxpcf777vzl0jw8r6eh32ej0s548l7f8plhr6dehjurfxzwq8xja7e0kq2tp63epkfyep2ge2ng0lsn3t22yse3hef02spmqd5qhnwzycs79ujn2030qzxge2vw6qlzegkxkkfund2ng9c92z5apv73a34dhtx7f8vk9k5rfmmp8wzw27kgd4qm29p6w369j9ja27sc9vcyyrzlu4e3ndcnx778gn0u5p7yzzu0mm5u2uw8mw2qpm6e0pfgp9tt4dzq753gvmj295qqcnquvuscvsd67ar4020de6583vhtelh662rvdwxshrd5g0fugq4p76j2rhqkucqegce4rffn6eflphqf38al8j4jddl6yyrd44f56j6rq2mlc3jy956mt26mlv4v36cp2khyp0pjafljzdy2kp7asgjzztfef9ej9qyl37kluf26k6ha77gnve4333daj66mt4gsu4xj6uaca2udv6qrl892fpctkp86tjeu6fwxpnm8ksf2rhvy79nsp3xkhqhrmg8ekxhucggmrzaftdzsl8dh7pzsyygzsr428n48thpgqzlcttx9zrcsstyyawkfktgnn7yf8mpdjpyzsawudtsgtfcplhvdzeakr23f0ksl9ktuyh8lshwsm99kv6g69rcscqm9npya882vjv7t75l2atw9vgq0fltg4v27x5txwwyrrkk4madwwye6scudzu0yh8zyfl932lryl6nh79vhgqpqke9lxcgczcdldc48xl2z96cje2vmp4gljnm6tr0yzpzsnlwu0regkp7gj9vadupc65t4rllg7zxdskca7fz8cvehxk7t24w4aqmjufag38kmr9hqcxu0eqt7hvgncc0d6ke3uv2t2ffxfea7tk8a75fhctl4ymjanqjjsgprdw2nkqvsgjprcs5j8gwcu56mw6w8545lmvv6ut3e74t5cdq3vhp7a3r38k5dru0gnp60mur7vpp3sqgpvddqxjhv5acmphjz7epk9qcm2qq4z9w7pqg86453pk89p4z83886ve7h963mpjg0sxk6s7pxtn3cpp445hg6zuhd7gemx49ef3rrjvecfjg6xq4w8qgy5v3sprldxd9zzg94hq5yztmcdt5wfqjxfzr7yrcpnuxd58ruhzusu43zun8vadxml2huaf28n2azl633vafgv42wefr4cnk7pg3ptk03sln4lcg9gzs25d2pyecdshz5tg9v5435rl3ct9jylc7rl4un97euah2q04y4af0cdsrxcar9jspkcpgxnjj3mngrr0lju5u052a477smaeevuzag6awhw88v89qljunvsqtghq6je5fntwjtamv2w5upw5zjrznw6pvsam382vwwxxel0g5lccpnnl6clvlk3j26mr76ch77xj8u397c805wswfra4unmkh0jchewayymwyga0nmq3vcqsayqduz3cyjgznt7pe3q59cjlq7uc5c0klml6ejkt5us06qgyduv0xg90jm8sq6vum23lg275sw7xz0hhnk6e2spk3aw48ujvnuqmrk5f3rhsmgx3dmwf8ecsh9e46kkuql3lch0vca2ummdgnju68v8n6vs9rh9akgzf48mnh890gwntkghm7es9nwdalzmky677l6zg6flcm07w5gn464vqngd2y8pdj97s0a92ndhrkcdkejmwh6t3lgecnh20fsqq2m8hy6mplmyh996zk0acnf4p8s8487tt4kjmxyg8xsy4dz73yqrtlyrp6999vyu5wzfy6ute9k8djhcjhw93pa6wzwfmafp5uqtgkrfgyy3xuqs22q5zx7cks05zv68k9rng3y9l26rt44t82emm6mghc9amvf5vnwhrqwucr7g2jmqdmcnasnehuqlvqkg45wjquun4jhk9uu5z9e2zjgkxes9td8ylyzkp8u9gzlsn8x9lwpux5h3e0kkdls0s8q9qv3v30r5mn7ggdtgdyaeqgxt3yq9rp35hrh0fqm5ct2mctwk9yfn730j854quuhx3qzu6t4nxps4vauucxtv8e25sgteg39p24gtpvzjcltxe5jhk2fwnywru9pwe622sm0fzddjdcv5trxht45l6m8fhl66lxfagcttky9zmsqs4lsuhhpgy6m4vn42dmzfcz60ygsez0frg3e584dtpfexgycf4x7tsd9u7xgfq0w6ka3xran9sse0ma3v7rtrk2xqpmaha562wkjxtulusrwkljusc4zyy0gxj9wf5c84852xvrq8yukzrkk46d7th8c9w2rdm0glwdftdft4kl4s9y7p42ryklqs9q0dveph6qdkp7g3r0sp5wd9gfq7yc5mm35yg9zcfumh0l5fpsh2u25paxfwun83j0w5n6ppc2cx99yawjdrv0dwceydr43f8dwhlafjz4d0047wuyapwqkfqv5yl6vhfy7lg0uutpfedpdeydk0856qjgv42x4lpm8g2pvdg9fn35vc9uwh09uf72e502ucl86mvvz9zcw3np2d50ly4dmf2zn66lmkg50x3x7dqjqxzzs4ye9nh4qcvunk0prnp58nh0s7lg8wa9vjg2f8300cneas5m4zzcueyd8rs69mrfdy64ctxl8vp5eupcvchvawdhkmuak5w4xykyl076qnp4f76u5v33u9sdr906mfnwusn2yy73evu96hl3htr67dyhwvcrjk40rlsgf0ysxg7he4nlttny5j4svrxwd85vgkspf6eptdc40tat84aexyjmc8m5axr3642vk8g4vfyc3t36q0zeesqjzhhvj4cxrddppg4klw2p3sgwxrcskyhj37q2le78me0y9tfrtv4vnllsdxw5g9gdtfshv4gpxh5xdx5zpnngusr8axyaw6pqtngnerpvhx2z6gs8tljlsh8q2uaex55r262gl3tupjtl6mw8wtdk3k99hcus7q8vvp9fuafqqkup568g7pfc9dnw5ffg2etxgg7kydfgmt23k2fne373eaakzlw96p", Network::Main).unwrap(), NativeCurrencyAmount::coins(5000)),

            // Actual premine recipients, added 2025-02-05, in 117436f973b3b9ce8923bae614cd92df59969800
            (ReceivingAddress::from_bech32m("nolgam1jak5hwckwysc0gulu6vxxyjck7p5q47yjjhgy62akvedd4x5pk9gfuwt0saydg7th39m69pcl0n55dw8z3f5wjxg6x34kqrv0224xpl0tup9k88txw298km4dyxzccj7quf6sjwl336uwey8kcynzks9anpu0wqj20z4eg6htakscads85kd6x3z3lm56f9dfg0phxydz7ydded8e97galflgs8wmtyf9l5r58z6w82q57jdf2skzsks2er8uxuuet4rfh666yej83lnl35gm6dg4plmzlr0xmwws80539lxpkfh08kswww67wh2qm7mapeke66c7t32prasscdu6zcqlk7fys6d53f56jpj6nnd7hl6t72yjznxusy94c43ktmhkl0e90radse88fnr6qhg2ar32cfqcls38793q75v8jsyl870gq8flyakypntsd5yswet5l0ps06pu0t85383t7sswkew4mzsng605mf2pjthz2cpevtk9zfsddtnns8nnsnuyguccnqca2eelgwkpyxmfn9txyyzcslsq4fqwylj4luyd8uzhgpxxtj03ky5aa6w4es3p9s0xc7tlcxwsm8u6hergq3ev7he50gs0nnkld3js24y098sk4gd8rxsestt66dhcz0z3hawqzldm3hjzqmvtw9l9aeveeyfypa2wtvdl2l48jswhy7k5s2cfyma9fw2g05jm8q5y5cmxyxj8pwunq8qrd9degxr6dvdh0tm4520ff0untxluyrjjp2eunrlv28ckus662g7wtldvg4fqa6dvc8g8sdra65ku0d8jjfrnrguj777ev74u9mdhkefx5thhm854vk7g5gu6tl7f38d85gqmw6uq8a86h4en5wq4qxw2ljlxqpass6l777vwagl9w4uevra3h052eq0lpmmqj3m3dm3ppgent68z3ss5faxxgsakc4jvxde2rt9t8vjplf3vtxszvn3rw8l9s36ltvlx8lte0zw59pqlfk7vde8aseu94jydnvvgcnm8u48fsn3f46qv7sppt76xvesfwfld3q9zmkcexesv2hhm863ztq7wspqxqh47qcfjxhgesyp9agzpmmkf786pv0klfpttc7plslw3696ungtd3m3agelmwdursytwrz0t9xqsh2d255wwc5mjhj0sd06dm8s2cncrca0d4y5q5q0x794jf6u7fzc26wunjq27lu439fsk2vj2tpv6x9khce0ffdn4lnhcvew7rte0dlrdwjzkxppgr4gh8m3uc58rf8xvft8ngp4dnkumgvdne4h3um0nya59r446v4cclzj887j4sa2d6re37y88rpupr80dnt28dqhqfvvs4adupyj7ydnqaunlkvaq04llyj0w30rfzx343vh4d96vkrk7h2cxxlakvyr6dx2t6gksemjqmm5k8hzj7euahdspqjdvczmt6ygzrmra5tkf69m0alc8d4tjqg6x04vyz0hwyy3xvhd3asz2kwjxjmrgv638n9ug72nf6q9ksew55508pt7euen2gzy3t9mhvaynm6qf86n7u3nfeqhtv8zrwhqcg9e9gmlc496kqyw86sreuu8qz3mgql04342f6xcf39d6n296yzvwkplc7u6r978lqx8eljwkyed3gw543sx3teav8hyus7qntp6nrcwgj6yt9lneksflkt5msxs8rp0qemww9njm2pjd53y2wvyj0v7ggy6gchdjaxkrwaycj24n43m6xvm89y7knayzark94ackjlhfx6nx0ukmclf70fyx9feu3p03qxu4g4jc4xx25k7u86yxsuqmyq29dxgth0a75lctcr4rdrsvu87ppvyts09c3gt40d2c9t6ylg3h7k2737gy8cwtspepfr3up30r4ntj9t904l2q4dcnhcqkvsyp7krglq3se8uq7yn25yencxefy3cs7utwcdh0q0lqx8lh7g6v4dxrp4en0t65turmyus3rlnyhxg8d2gs08mjdsqtpw6l8v37ar6zwqlu23wuttwmshe4x4zpn4rp7ghm4lcrrqajp54cpf93z2jujex4nltr2h97j99054hc9pqsr58wafavyvmd847ztmeydhxwwfthygf6x0chg9440jzxj6ccr7ama8g2etd2t26429lyu837mr0t57u2jcdyt54tdafkes78wuphkw2wzkkejwptm0h8rku66kdn5e34m407s69pk58jhmy6c3xrqjwxmlf2flnrryvw3eekqu3vxvnnpyfzs6qehkwghtsu2rdu2tanmeg08ef3syywjhraasy9x0lqed7yx8rwmzsq2f8n5x3ut5qtlq9pac7sd0q2shpfpkchgwyzt24gld2dxwruvyjq2ggfhqeus0nrewfz9sww5f2ppxtq0n3qgtndyj6hp8mmfdk4ytad6gmukgjfm5ke873m2s4r0m82d5zvhdmwwdptkmp5lpqv95mzh5etq3r5gd0l8el7ah9alj26chzr3l2ljajmddcgklrrzegy48z0n07dej9kyshm98rmgahsdr9unq85cq2c7qs275vwdy8drqez5rydvern8fav797v99wkf6qrxdfeaznkvvqafzu727y2yjuftheah9wlh8fq0n28rhmsadlp38l7ncr45acjy3xpjcrxhxltxyp95qs80anr7zetvxrwkmknz4z8y5vgcrd3d5nuesr2lxg7u7sgramw3xpgazadhkcql7x6trwklpf7ydm553zzjg9f560zzx03uwpxe96ljmq0upxf9wgr5qf0m3hafmnmzjt4wzvu0rvgu2vh28ggfqlq3aveqwxkk936v595eck2k3fkdk0fs7jmpqzly6qg34ddnyvu8tt5lda8cu2g467unx7dxrw733v4p5e4h6u2retp7ws4ukzhh5ajej9vtyxf5nrxkvuw7vtxtjka3pfh002lwry7pjq9jydtwrcjdthhmc70sh4f6mr990scf45jv0rfraqwzut7dcarsulpwv9g9a4p05zd2f3l6knq6429zhdzq0rt4h43phefg79whvrtdxce5pynuvxffgpfac279cuet5ga9mmy4mlw7fh34cwd82kn7hrpjvy45x0t0vszhha52v2y69r86pa579arg74g47fa060zrvv07eaq7hayg0c3gp6xpayhx8jhv4hgr94au8z66x6vwwtp0p6z94n0x6y8vkkmvdj4wzxcs3zcpdmss0kngptp4jw4n2w2vrzwu5s89j45703d60fqrjj3d5t3al229grf5hq6lf0kmqjf4enzsp0trhkdv0skx6u3dqks7rv5ukzs2puz7ma8y6mjqpll9wgwdcu85jlqglz4mx8du2rway5xxdaweapsmty4rzhd86eafq50xez327k5avhf96v2u72v8ddjd7c2d258yrrxf9x006", Network::Main).unwrap(), NativeCurrencyAmount::coins(3118)),
            (ReceivingAddress::from_bech32m("nolgam18gumu54qqvfn8k0k9hmud87rdje29m0m44v2m84z8dp4ujzszdy7qlg2j5utc55rl963vexk203uz4cyez5uxurgymsuhzyjqv7v7yf9kymvjcx73fmket5l52z48y4v6wpmwa9lmlywhvnq36m8l09usttjcmt4x0znrj6eru2ka6xzh99fr2d7t792wsk4fzhvx7pe94myqzcxrztrvkm74gj3f3lcuzue3789fjgx5nny6rrdum8xlwda3rhgjtempuv6e3u84wrflfrnl57kxmxny0c4neq4qmputknuqjqxszmsa4lfqt6z65pwjvmmm4462r8xh2d9tqqfc5zm6rtahvflxpnytvemztqzsr0el3d9xvwm84unw3xdazr4pt3zcj8cl2a7jusqhgzvpexck99elnj2a03m0kxd62nm9f7d8rn7cjmv4zw2rzedmj3me9qqh33ze5lec54v2urymencv5gmw823d27udpq7nv580jh95xuw6jkjeycsvu9dmrgp4es3qzzvdlg2jp4wes8wm0cdlep3h7du9pautlqqx0wp9yy7074pd8smncwhjf2gsrdqsyv83y2pwzfk9c3h78p62mpcxu02kscxzjgavs6up907x50tws7qfpk8v0477c7ydpycwk5xtuwu9900rhdq9v5gjznhw6a4ddld6yklu2m2xdenx3l3xttw4df8djqxfknllj2cntx58ehnzk2azqyg53czzfa0s2nzvjnnque4hnw0mudj82rrtehg54g7p4wuwtqejklfkh4z203xwlkmhcjhhgxlkvsclhvahwcrlxvxfmstrzvklnpjfm7x5jczvl5zsmfdyecjq8kaw8nuv9tv2xgshjvcnpflzhqrj3tw98crew9wdwtdytrweuu93g0tckpcu9e62s24dms07ptt93cg6rsd62uysgmymtsx73zyn7ehltagtxunlugz0wptjhqzg6u9n2mtq4fwyr8jdr5w2gfmddhz3qe3d4mk6ldv4ymkcuyucj4leen75gzdxs3z458pku6s9zfhehynpvmdqk35d4mznee275lq4qp43ru986xhqsm27x7vgwr6d80c5wq43fy6uhcdyw7qpav34twa60halq68pk6p5u28zmd9gt4hjsz7kxc3vark9vc7fz6x2f8n56a2wpushwt564hjvxy0nzt6syesprh2azmxgaqglhltup0h020d435evw9mh060dmtwt5jq2rhn78g9sh34hz4h73wypg2xn934x79u974ggyfajmy9a3x597c5lz7k5qu98vzfe9m8md60y06fexfmej73c6uzafyq0rnn05csh763cehe84q03hphkmayxppl2dm25njsjcry9fgf7j639mav5mze6tzu02xp9v8ftzh84t3x40zptr7yjyymwmmyfs4lytvj6kf89t354jjrqxhfj08usm9z3tdd6alt7tzvtjrq5643pdkfnkjhqlppz7a22ldwz9p95lh4fkgj0g3m59uq0je3rc8n9vwnpwf667gxjf6z823eumfv43ej40jcnup5c7k6nwm2kmc8rw8k6qh4qgwslwf0vxlyj7l4djp4kdrcv0s6nmzr0cu2jpv94874dwp3d88q6kama0m4hq3ry36twqymzjwr5gsn6lfxfnjlsls4hjrs2t70x6hp964p4zsulqr9kncxmzlpcyhuck27uyud3lqlyz4nqxskhnl4qus03s62v38er7zdm6qllvg76fymn7agfpfrl5jn65xtx3tnnh6n0urqpmd4unc7j4tg403af4squ9mf6qdk2q9a0kzu4zzhd05da5j83atyh6kpnsgkh2lfupv2agjhknmctyg4lt3r888qlf0mgrh5yddj2386ghgkt72ef4v2rgtsullmg6593tcckfw2p36pkykxdpcggk0hkt6awzrtt9unl0kam2xsq8696vpmlhunj87tg6qpjfmmj3skr5nhhjajn3yas9u6xv8n86xf6fvk5sjvmu30laeqj69aazua3r2wq3t2qe3fhrtd0pz3xhpcycjsmhhzcdmr3z8sja885066dm3w8vfam4rg5tgutmvvrkhd6endnhqtnmn0tpk8kugnmkydw3we8awal9f38xyn5q667hv79wcvsr406htjhyyx602lp8mau8h8n7kkv2zz675qh4j8k9emkc5a5798mgx7me6mq4pchh5fr485kn3wqqaszykrz99se0qua89e40mw0x3uw39knm3v222e445nantnrjapr7vyc82e7ue3wwc87m2nvsux24mh9m8xj4wc3p4zjgc73chvdyjnnt4auch8dqhdsj9es8mjupsevnvgf96z4xqgptw0a96g8xjdctyw8waeu7slkp93pzmjsfkdnw2fwuj25z05z0qpumnqjfdad09vhzlkzl6wjppnu5x8uvfan8avm3h6q4q9cmx04uysdg2c5a0m4j8mxcvzt0umgdlqa72r0rxmdt88wzgclmamzwwlsc39rtlekv7q3ayu9pteg4zlf9kphmmwkxfuq8y9xd08vs37mds422rmlvs0uqn2guxtlc367mej96mjvnutwwkqja7rg6q8qr5rwaye42luma5h6d33jyc93xr04ut5f87vpwp53pkjc4njvvhv84lxymlgjelzguu5zzc6l56sl6kf0xzjvur9cv4rzxx8ww0mhs5afv72xn20j77q3m4f6mrg3qrgnrcztcy7vcnamjnsfe2reenclcepqlhtaqgrwtfc80tan7yg9x7ql87ua768629xkyyuwzasl5qu2spgr5z588902s4kee5adsqjm95vuzkq4xchwnn442q5s7rh0973sjwlk7lwtn36352kpuqcmuz2m2840u8dytcty0lqrm669n4x35nl572f33a02yys5ww69ma2tt9jp2mp3z472a5vzz5q9l2tp5qp0fzf7r4m50pddzy22ten2v6wvsewv4vs2kx2jyqx5e4w4v7u3v2t6uj2wqne3z5vgmvrskmej0je5gh40uctzwyguh7cytr0es9xkz3k4amd2gw9xprzx7wrx8gy0pqhspp6qqhk74xt34k5ghmkrn2ujk00lel3mymvr8hwa6tctkezh24awamaq8lec9heacf9a5yq55vzpwphs5a3mzqdw3kr6plv9erqt2k3lm3jx5jhelxzc5gpnkkkyanuflwvr56ucg5s78e2px8pjyjtl5zpuqclujjd7kndus2sp7tpeyyqp0ne5m4k5g3v6ww9qk3knumatktazmx2pthh08st5srekunfzr2at8upcwscjh9u2jdtysfd27jj7njrd330dcmy8ff3za53r5rga0lxwx77nkyq20h073f5t7tsy6nhw2fj2m76uc8snd886e3kt8l00y037l75q2d", Network::Main).unwrap(), NativeCurrencyAmount::coins(9354)),

            // Actual premine recipients, added 2025-02-06 in 606f8aa29e4e51a4a11f7940ea5b5e16a0e9f21f
            (ReceivingAddress::from_bech32m("nolgam1s28nppscrncyscne0zamzkz9d646kpxvt370n6a4dvlzjy9gl7xdq2haf8wysngh7duh0kvwrnhc3y6hccw4cjlqetg2n3rkmshtzhuwk00pufc4wwdjafx79w4j7f8fut4pr87nqy0jz72xju02vs2wvcx38jpknnatwtfz3qvh0ygr30yv20ztasp4sw30q6qx8gmtvvsppdfgt6amsk7xgdza4d2x0x42eexzy7cr3dfyqs8aa5ws4a3e25n6uqf737mf3qmhzrsl82y0uks2sglhtzmz77szza28tuddxuanj5fufgauyjjzaa0gn69dsevd0ac9xwhr7kf2hmsjfdml4uv3d2el72e8thxlrzeatn7n7xnr3xuq9ap3fy4dlf4hl78vfj3rtnlqjprx5ydcz8u2nh95hs8vcda684k45ekjvhj2y3kz2qry2xqtkf6qsvu7qxnacjvkeq8kwkwlh4u579exz9sexwkpg39slg3vwfmtavrd7zlmkq2g4gth6nrgdtwd4rrjhu9q2dk480xcmwhr3w99kfv59mmpsu7x0xsta98s02h8mhwk76wz7uuxmk3wc7yy57wkpnfghs4hcpl3x0ejjy8ruu7rs9vrn9huw94qsvyz6e7ee2rz3k4c3y7v2g0hm07exn29vy6nwrf7vf6w8kxsjp28qewl557775mrz877ku9um4ufxz6yh97krnj5dfw7l8cswhmsa0agjwen9me6gew3dp3yd5wnnsgktrffjtad4pzwa4pngdpjs2hx2t33lwutk8jk79a6n3huqqf28jd347hfhm0hcxe5u28jm2pn3fcppk59kaj23puv9ecjhmau60cykefzfnuwmsh8jp42xyn3je7gsj0s3eukrazeuvtqh32mnegy05yzejk2rd8hrz080pxf055vd3kkgwm2rkz3ugzt43eehhvs9ww4vx6ceafw090u7a69agkvrn003hnnvwu66lsn66z6msc2s3scnhf0r8gkulkluvd03fz88lgy2qhm5ljyzve94472zcsfffktmr0r6xuwvtlparrn84hrxew6wyvw5myck5zgfsw6h9qx3ph38ej7gdgqmluh240qh845rylpj9lqpvqz65xdtvuman3mvtkaukdvmnst7sgqdlx8vnz399kt8q5v3rezvxazqk5mykc2ar7jtvj0c957th28yxyjnptx3na5wuxt2wnyek3yvwjyjdyg5309e5nd2ye54e47gn5vdgajajaansqsljxvhdlp8e4vetwpqap0u2crme0cn400x7ylp24ulg9cv0w5rwstwgf24z6djj743h0p7drs0cua4qta6dlyngjyzjrhxk82rutsgmscp6h68w9g0cpw7jasghhushq3h5vdy0ka3a20lc4aftem3pqae85hlshjacpcslzy3vpfg0jj3uzvl8l5zfafm9n8gwdr82tfczlhf2v2rr2rknwjx8zhzx3xhyvn6zr4yenvwv4rhjqjjcnz6egqxuqx46ye7t8ak2e8hfsjl9jrvyl6x42k4tukgu2up758ehmt30hmz4nmnqcdlch0uxhy9rsau9rwplkwp4r2zagvqz7vl5u2m9n5yqsfkwfglgstrrkxsc2w9rylq0tdgau089a4zrecdft4mdx65faaz4uj32w0tymg6mj0n7fuckvefe0t9p4jy4a8zt4upqxm39khh53aejp8ycksn8tk0muv49tl7uwlxxmh09pj8scvejvcgax9fkn4sg4auephgf9hr66hakqu3e8r4z8jg6pq7h40z99xts6lslkdze2kulaa285s3kwuzma9udsv59dcv2zh6d3a2pnqpg9pm57dg8gpp8x8e4juzxlyxmwqud2n35mpnyre2ue334sfhcyp8ppjf07v6gdrussyclrwt9d5t3gh46a09xry5h2ler5a05mfqn0r3dr4x5w7vuj7enmrxy680m2qz7rzv66vfshdnclc3af2ut5j33xjlarv4n8lj3mu6uw9lgv4m86gzvs39zh4ssfmlrwq0ndl90x63n9kecx9ns42gqrltcz3ajtdensy04myg0cdwj5yszqcdlarevkpvvfxjztstsa2zt23ld7n7pjvtn6yc74gcsz5kkv22h529nx752curlj0rq5v4m7zqz8pm774ak0733v9ga74fc5fne7aq04w6asrfva8y8nlg78rqfptq7sa5n3t3rk8438sd0zq0dk8vay977t4ey5n4nye3k9lrw25nk63qucskevucztqz8q7w9g4wkjj93zjr8jxc03nt7fhtyne6m4a2ee3cn2zmt6rv3fsxvt85xzujw9cyk7uctggqvv4f5cmaj0w90xg7gs6kwv9tmv27r8txu598ue59mrx7g7cncypwuwmwzfa828vt9tvh7pkvdewu4layfk9zl23jjpuj3zfs4fhll30qlej3s9ja6vwmg49kmcsv6yq4aqlwfgd7433v5t3dhv5yd9fxhvvq8zsczpfp0lq4ac9h7m7hytyl8qm6ztr3cnxnqx4vt98xhc2rg5ed3d7fmrfmeheq7xrf3qkmua343530gl9satzjmrt4lhx073y8dhhcp0ap584qrpalywggfy8vmfp40amnk2le6rzy837fca63pzd424ckld304d8sm2tpdlskp9xgagms8d9ak50leww4jkcfuqf5ttl4ajhht4m67wvxqwwzg93u4eedx8r82lffcrxhl8vq0ufpwgq3exq53drjw2ut960aezmmcw3zeq2ay3meqv8ysw0e2zkpx4xauhaghqwnacqv6g9gvkv4lufpda9xwj80mxhm2lh664kf8xrpc3nu6cjg2xzl9z698vqzzm5p5g6558zkwx35gk78590hsyp5jew6jzenewvrq0shr7pnys9hl4ruc33pwrkzf5za84vkajq6yfpafkqnlsv0sg3v3j5us0lhfsln59ja7ge3fa5fdqrpvg64aajjf9f2e770yxf0qkztlrqx4amx8xqcczlu22lrnsx2g0gxn5gjnjryvdf98d2adlm4l93rnummsaac0yepj8arra8j0vyjcznc8d8spqgjy6spqskr5j0e69yr8p2q9tnl9z0zrk7pd9lpsr5genvjujxzmayy2qnzjkkt9d6aqz7grm4fl4qnr02kkzt23zmncu3ey6nfjqvmwk2zwqcegtm5t5y9fe566fpp5x9ds9sj6ggh9xu42vtxkhwfy52ys6mnzzqnmhtfetzyj2zless346n704pats9xlen3v3r9pntvan3wklp5tld3hdx77x3ek9t8vquu36aaqskd6k0jlgkvjsfy9nuzvfn8p68dzpyvyquj0940a6vqrsf77du5s02f3m599s6tqzsa0tn9lrla2l2l3rxwdxkjvve27mz9e7scp4kpy8", Network::Main).unwrap(), NativeCurrencyAmount::coins(66667)),
            (ReceivingAddress::from_bech32m("nolgam18gswgm5k78zzr73y5tvsfg6lhr9qgdll9teevh2640jfve79329p582tmj0pj9pqs7kuavyscspw03068jy4t2jxy6kwd8tuaytaprj9clvsnfxzdth8r0jp4pfycez8caa684rnd667xwtyjhuaqz99ne904sapr7tgl33c0wxvjmwnevurmpq0stjzq63v7rh325qrvwrh8zrdhq53kdjpvmtkxndsqr9kvmp356xgfatexf4s9ln8r5n5wyxkcm9czhhk4nh805hzzfzfm5vcj7pnjhflsc09x0ql75nukakdrn8g8spwzg9zqt53jl3yg9083rfjpsdlkestrfgf23uufqrv32kqa9mm4aswwy4dm37m67hg7gc3wwacgqxdss8afqxl0u8e7w4yuel0c9uzlm6w45r4dp2w065zdnjecpyp3v6ykn4w28a4hrep8kzrj8uflw5ucaz2p0d0csacre83ce3wduk7el69588jtfnqs9lfy09pr2c59dgp9pp7jssyqzccx8vzyv7cl9uja6ay5eafapyzefld4502qxrksf5f6384yrqkyuxf2xf3vrhkschl9zqgt57wfx5zw4t0hn05ad63lxzyz9jcaq36a3ut2fuszalxhxlfq0fn4ujffyay2n6smt7dh9mprdnaa7yu2k6qnhne3x93kjvu3zn58wms6z3vjd7q9s4fn73cxy88zl9udqc2wpnmj57mycgkgda4jetxtwykw9dmvxskkkzuv5wpehz3mu5rsudfrdt5m47x7qtg9ack0g8w3nj2pcvpc8hx4t8y92qcm735neelx2pc2ymf3eq7r42resegfnfaelpyp8wwnlslufgeh485dzrrwwx9mlldlhsxac6qa069qzwhcch7n4gxgkspnnzaqreyv3rj7ctd6zrnn7llq7tqct0u7jhvyq030yvusaketecf23z7gz40wt0l8m89lc9wra43ky4efk9cvvgagamlqfcam35pukfsp505p4x0nwv6a9qxy9lnhljdmlwlx9mxqrwzwr4yhrze6m8f43nvpf4yana2xwv06973axvtdm6scsfww6dxwpe0nedh3cwj6nk3qxxsk0yms4dr9mcwymy4qrnzawhx29ts8khxlsnv7mglacutpdgkcmqaguaghuhggajln3ms969dj4m2zkc0v27c80954m6ag6l752p9k6hw987v50axmknskur369sc9ftw67tgq8mzw7q5la6vccm2efhrfc93tqn52p50hawad24f76zz7rfpkgk3vmk3tattudjnlqcmynfh022zeva8w8nzla2zc5039pqs6pkp8dau828aatrs3cvprgwr7njcwykxnxf89gze6ntxarcxnwkynm5n8c02urg0jya8sjfez2cxs77ns6r6pznk2xsty4du65ku9dl4hflzvuhlr7pk39nda5ush6s3emcv9h5tayjj5vc220pyaayelmpmdsfzg89nuc7w3cysd7p632f08qte2swkchwz640pkj79k0tg7r7qmj8nxl3qkslav945davfht5fsnvlghz5q4kkaxp9nlcj676s2jt8w58dspr0t9yptkawjjysf3hw9evf0hfkdgftcmc77r9f5kj6n6k66jq35lcep7yutfmkkhayly9dejc4d5kkh559ry32gmqs65ap0du8jf4gqkrvszzxa98m9rj6ju39zrcvhv3k7gcy0l6668yyvx7994sjey3rm7568vvsxsvp363psshsc44x9hcn8tha62rup35a6kda5h72dy04gppznrtvja9hlvt5rlq5p02559g55u3ws63cecllszh5l2j4fsztnga9xjel867pfjzv8e0qvj3ckqqugp93lunj0ywpaytdj6nag6zt3zpc8yv47p4tntzp2f7engphn9k9chwg4xaenzawrnxlr5agwuxn85kktgkjpwzu0z3eny6dcha5zrtg0fekdar3242cps8eqpvz4m7xeeht8vu75654j5n7phy0wh29rvt66ezxekrk66p5tgygwnqg85n06j7j2kdl887r3rzc7aystw797r9lyfrkejz324qn8m9umfmctq6yckesl7us32hr0zkunnq38rcl7e7kr7p36dvnpxedkc820z20qrc0kgjkep3d0exzt5s3uw6u8neelxw69f2ff6vp3dlz6f9h9avlecg3tfp27x6ne43j965g6mstjck9nwy326m67fq565d66ew44gptztqd9qj8d8rhapffmdhx48cmw5xm4fnfejja2y8nz3p9vx5ckfsqj578x8dp9alah9hrpprx8qwjfqnrq8gtvlw54earscjd6wx63frm0ftm5c3am7uqxwdpt4q8dlskak7gjxv5xp5m3h6g9k3yw05v3t682k5twzvrewqmw3hrguzfx2epr2ez2p92wk4v2cn3cfly9pwde879s76n3ekj8qyqfcddu9pdy6jc2z5tpf65ypvmya6xudgl9axz5z8eelamjent724c0qyhd9zqzrxls6qgwsdl0u2laqqcu37mczjegscgdn7t0r6lt70l7ftdjqr2njg09pkuep8uk35x56szrt28t5malxu6nnwzdjrmzxxqhj3rnrg5h9mrttv6l5x7h9du0dvsg92g0wfg95dsr6j7cwlzsfsszkdjsuhaljtfk7c29gj9xl3dlha8cgtr5cu03dx3cve6ctlcdf48fkzs529cw2fknpfkh9acupf0dct0rwfay0hnvtldhdfxvwdzjw2mmuetmvs320ct0d74xyz4x6urketa5m2s3ycmd3vt98yfp7gsgv4nf5uh3gsgvqfpqcflvsuygfnlsrgst4fgdqskmzh43w2grsggjnwsam643gjfwn8pk50c2swuer7heuxttlpjjwkfmp6syhzg0t92rwe4w7f5qx0w6kgke97t7rkytugs62l2geg48c3ujp7z5z3aqv9kdtv9mmwyjdc4975v4ldh8zflckc8vkuqlspk8twdvwskucu3wyhv4vudr872ml0fas9gfe78vjngq2hvexvk6x9ggg7vy900p00sf032td0fkl9w75ldcx2q9uxe0t99l6qsnqk6ygmhh6ddapygvl88m3gfc5q0dkr0ua3t3z7qsnfhhp0a80qj0ugjd50zqq4jewcqhmt49ca7zpmekrdetur4elgqhq4g7ltsccpslchqekkrvwdfkplhrd39nzs303q5s3mpyzf69fyc7tduwfjn978l70w2er4lfrzvuh6fzp9yumhuxpxyjgyh3pm3zzf2wh39smmju375djf50zu4z53zpeu9zj7dvy8xw0mq8md3ngnz7npcs5ru4u49gpm5vmwwkeyntafulq53d06fhqtcsggfy7ll0tnpxyamezjz2wa0x8nmzzxz7ufa2l0", Network::Main).unwrap(), NativeCurrencyAmount::coins(66667)),
            (ReceivingAddress::from_bech32m("nolgam1zyufz9qqej6xl35yfwerkv830t0xw67a7u6tj7p3ym8dqruqyll2xfjrzmr9y9mdh0at07mzncjuq4x9va6npme6lhjncf3clrdkmkjap8qjzktchx3adste0ckm5w5y5fm5htamq8phldr2gz4zt9kcf6e29hu7mt8ssje2v39ytk6q7l0uycrxjskxry2llvgkhnfxtgygj395mxlv9vghmkx2x3qasfy90k57hdsfqchmgm849vs3yev5lnzk7mmee0f4dcyuxurj2z3846fhqjut8k94cd0lt3u0wjc6py94guhat6u5mtg6slagzcq6yvc238f969gjfrxrjvtdvray7kg2909r6lyz48fzdgyfkfuhrvn4e08ny4974r37le03pvqff7dqh6vdpq30fdy4kdsgpf2j2ec0kw6enwv8unc5pa685g4rn5zn3tukd6cg9ussha0e8fvvmm5mrkrnlx9khzhthl2x3wchutduegkam2lm88dnvsf8q70dmvtw02wlwz38asxl5z0t8x9zauxzq8ktm6wwwk44lhy8hfthxl8uwccmpzg64er3pwgumvp0p7yzwree2790pd3a40mjs4uz34g4w09eq3qeh5808w83wy4pc0lr92uccxlesem4p2z8wck3hgdt3whjfzlvc5rdhr5f2ezq9e387wgmdkxck4gft9qarhzltdf64qty5fmghhzm4wlep9g807adplpuw673n233yucnqejpjknrds0swwqzjvartutxh2xr6eu2ssw5lucf2u0k0jyr2khl2pk5yapacdy9nly74qykcxc0xrxqyv7ycve0s2t4n9wmtvha0ahcfcr9q2crzkhh9lgja9uqetq0nvkhrmhqxz6kcpjquu0jvgww8p6j2gspflhqhwf2659kt0nqxh5jrwxk9ywx792a587cv0ey8jg0mlmsedc3tva7xrywtsemq9w4zvyv8whgeuathjsxpm0md5n26yz8lphl3q492rld6q6mmtff2792cfxzhq8c9cn2a5cv2z2wyazzwgp07z67gtlv68tgd86mnty5skghgrl4knjl3lhnvs3vxqqlv0dx664ythjkwfqth3yune97zdjkn846k3fuvc2e32g5wefwxus0ttc2pa9ft7dj9l7thplpwzjtr50g6cv4xvc664cladxvkac0h5slw23hlxl7yjgknw8v297y73psnps6r9snr7dvfyz5sgs4ndyw28a8wv24uefltlvv6krcy0zdus8hwn3pwuhh5pww7vhd90ggs2tff5xc2z6xtgt7kv0v57w2vvsqydzzagm6hdnj6fqcvau2neza0zfw8yqvgegmyr73gl2r00evqufkt35rmxzm0we3yayml67laa3qzpfvkr2vj2nyseyut45whflj2a84xxw7vquzh4fevt0ls3ugyy5v0xnu7nkwg9rj6j68kjyj5tksdr7h54afxk86r3h2y0ce8r2llgq0pavq6xxmu55m5ygv3hqq562utcy2eks6rj689z2x7m707dn0ndplmcnfezhq5648p48mc4dkch7wjf7sggfr95tpyql43y69xc4xp67d3sj3w5lcz94h4d5fymvw2rn3cw9ge0e7zkek9eahdvac35zvrdgcywtsqq2ssv9me0k2tua4js9vyrgkzhsqqwhz6qm9lh65z3gdfzrdvpsl6tuchnmp6zc0ppsan00guvzw43lhy2nwkdfksj8ux847kdn283tmech8y4wz4yvl8t73dsj8cxsk9xcgmn44e3kthqhw6ajc7ghf7e96cvq5nxeuqqmwfk4qmhgnmztzwu9et29w4knssz3n6mcvmq37xx4xvyracqz2pzklvfs88k5yzjgcc0pwux5rgfu7dlh73k4cqjuf9s797xv32mqehny82uphymedmc7ff8r7752v92ajtjuaav3djwxwv8srsjpftu2z575ssgyhazsvrlgeqdnwdf7xus5pgl3arp7ruxgjuggz78cj7vzmc3t5z7krkcaf4lsxg0zel3qtltmvkphzwxj2ncdqt3f8rquwlmcs8tmgum7eplg2qfawq55glxmrkey8l4whgxpdfaqxuus6pkapyahcyzqw5w4rlepm9krm6aqeatp3gfry7stzs0vlrp7lkh26px8ash5al7pes6nhshscef2fk8408aqzddys2mq7rsxu2t002d6njqd3tk62xpwxh6ehqlayhygmmd086lvm9kad80uxlnugv0hdse7dhrv5htnkhaa0trjhgjs0th5989jcwulwzs88gcdpu7ylatr2fg4xff0mzdwndkuuuxn47vz7vtzjqatxa4jh3eq05s0vd27qjlvrt7fluhgkqa2udrsvfm8zn4fxnvkvy27cp4yx7q6rqp2a8uqe4tl067xk8chuzpjts0wwx8vq0vyvr06d3h8aljc80gvt69kedatf2rsjqahepu049nje2pg2mmcfmnqpyptahtc0zc6uujkdaeq5up8fpclpr0m3fgnmde7vk99dsmumrzdfgjzhd5q2n0zykgahymwv8vcyyt95aw59htlwrrmz9yhhft2svfz5jmrhxyyyw4z8r59rk09trdh4wglr0n5xf7dfzwhcxaserseryjp6ty2nfmjejrh8z9gwxjzethtka9rkmqlvykpuh003502padph49ad5afq4pv5869qaxmedk79f6lqlma4zt4q95nfqghw3wf7jes5wqs2849j7528w83wteast34cu8c0ej83qglvkz8mup58gzl8g85jp9g33l6spc7rnhh47zlv0gwgnwv3pqh5mcu05wdwk56c3cpkyfc4y2ehr3qplg7e2ztye3g6j0jmjfng9gznnhmn6p3z9akapdq6xxywvf28j5ds975ndqph9j5vspy3le2vulgw6rzjtqxzgg3qn8kesuz6df5v8nyhukxzgdad2c8ujfny5th20hvka98xvhpq02p360xet5gr70y7xng9jr0u0074330w3unky4sc364x0qjkdaek3ekwr6aya0kjv9mjmw0jzaqua5gxsxcr9sd4gvl598z3f64weamuxmn7xsjg75sl9nwvmphjrnfwpgp2knhe9lcxgshc46klr5zfrr3gxcu6jw8u4r7m6su8mmt0kcvf7f8xkfp7a943gef6w0ln2qnyzggslz3vrddrlc4zv84jl5ajulhwz04tmrjs0mqd3cmuqus7g9ws7l4wpngy40jgwxyqvwzuuwwzw3d8sa5882333xjrx0sfxpwr6nsr6slyk4lwrg782rql5pty4smq6kgsqwxl6ppl93jn24wmeryxhej5j7m76mxp4r7v2f7yky8qzs2tc2fr8x6233lu7ku3g2062eyse0u9g772evmh66dzj4n7p8lrk2vpl0e5s9d83tt", Network::Main).unwrap(), NativeCurrencyAmount::coins(125)),
            (ReceivingAddress::from_bech32m("nolgam1yqxqs9lxg3rxrjpfm8mx6x8dl3cfnzujphnaluu9g032wa226cpafupgne3lmpv4umtxjfz6qut29yxn8v5uzqf7aa0f7u5ujvs2xcutm6vmg92g4nkyzdr4h5zeqjju5j9a8a7rzm4yevg8k4ydcyaa0t4hjjuph7rujwzkgqwzd6qxacqqn2nqw5l3lt7m83ys5kl8tnrvtcf6mnze7wtjnfc04ukay6qt048dgv8gv62gw4qus9eyhn8eam80k8qsky6uh86zvwmr4ajw5expa503ek8gcwg6enhn6j8g0d00z2algqq6z9ue6hqj28mzqrpqgngc67aw0yvkxf7ym3jq5xn9qhqhuz9y0gdun6q28ae7ch4u87l6zzwu3gkmt093asde4m63295yd74775umtc9vlkakgvxl2m5ufmlkuhkpy45xttf5w23n2krfp2kahh6eygfqw6yz5ed430waxnq0zaljts85dqcjtgh3lvxthgwxxykfe4vvdxy9xghuwwvezqt470ak5v3cvme3emszcnr8tquhuwez9sxzztcutxg4v20zcmxap2ar4q2nqf54qqjc85reavtmdmg08k0ng8sjrc9rwmggrrtq09fct76gkarcslwtq9t7mr2qv249a026yx9xzctll8t5n3anm3ks8juhmlavhrgkhcd5njcmrf3xgthnlnq74c7yz7l6ufl00h6eth2d6j0hv0z8ml7y938ade283ugya96lq7glm75pq08fy7a54gl0ft5qtwwtwj9tw0t9sacpplq5ndqqk28qvze9n574cz76kqz2vyxzhryg53n6vfsky5rt939p6zjq2wrhsc72zyymc3v6fcv0tp7p54h6zq8eat6yjs460gpjsrlj583zy3mpfqka75gspn7k8n8cp2043w9d0qyat32h0ssxh68zqfx7yj9z229us5nkjvmw47qfv46z435xfuacz3sg8hed2z2umza7apw78r7qkzm4zcckryuv6np449yufr3f5z3aavd2svt7ufu6nv0rceuhw44rma09d2vuuuhqyqqhagxw88cfjq4c4ddn4ey6xq9zraar59qhyfuns4s7v8pk3tkvlz0g8mvumva2c7rhfe7szxw8gaazlnfnzlmvfnm5mdw9vvk5pd5rxpey632g3tk2nzhl08ht9uqgsmart928yqcsfnh248ad9fg7cyq88mknapdma7qwadcxqpwtzq0n4dtm3frvla35hd39m0yu44fwtsp82gqv4yhkazrgts0p3qjmcpgj5z08gwlfftqacq59lmtk9lkvguyss6lu4xv87lu4mtd9ngfzl05u59nyceuh0qczd966tsgl8x9k6dv0z6zczwg98kky80gmfn9d0y9ep44getgrq250sd4gxx944z2m5chrwde0ee542exx6n269y4nkyh7acwd7muz8rd4y7n4thdc65azjfmnr08jj32hkj4h7hzv8jjzyg0g9y37c86hwzkzu69xd006jts86gd2y3pkyqwsaz0nepqp2uyvkzq8nmaz54uwd7lpm03ejdf5jm45eunf34gzfgcqcue5sx236qhe0qhxt9j3jscn5wkcsj6928n35sehfrlyzlayke42tzu87tvpjqgrslunln2vypg43lh90cavu93gxtjhsd7w9z3v80s340ye43xgql8cf3artytuhz5fyvctlfx3pp6tsd4lus4ptxqv37d8upwpdklt3km5n4qjk5shy4fjvy2525ddjuw3agul4mtk6xk8q3ngu9ax5q8wlzr2ddgd329a54fxrfnkt9q9jkljlzmsfe0cfw83d2evnz9nfznmdrrevs4gs30th2wrjmfxja37j85cx2wvwyp92ezlkfpkdx9ty4xaz239s0fjyqu2sygy5yyky0lmar3hhl8ph8a65zxate2sapsrxdx0z4ql8ghnndmh877khjw0umu8her3tttrlkkcv799lj38dmeujuavpj2xl2ujhe8l3s65a6v9nzjvnp04khjk4h5u03rvqvh0gr9tmn9g4rkr4vc47ch4pcre3adx37wrgq83cxj2gmceqdsm4sly6kafa29m4867qgrw93j8djrhmmlcdv0m0xx553f9d2txpt6jyy5a43uyyhsm896zq6h8ugrwr4ujra5up7h9dej35zt7kad0xgp8fwwf5vsv0290ar7uag4ypn6wks7zz3nskuumy37ct6cvg7d4hx52qu3spl5kmqmuds6jhrdexfgus7f9cklwztkqnf6k58x838v0x4w7ry9ys4j58axcaxes6dhwdtcjh4wq3nj7xk62uh362gskq7nz72g4exdfur3pmn6na2jcs7weh8wyp3rrxztfyxsl7q5245j3wvuxgdvw0qu2lva284xtf5jltm80g50kytgusfdn05fft2m8fr036uaqjmzv0qljq57gk8sxklyd6zgfszv4nqd7m6ncskszcsv08z8hvekn5wzn8ss8xmzjseekvwa55zu2cq20t8m2clavp72plaxrz527exjynkklkrraj6mk5ch0a5kk4ru2s65ylg3ka7hrtg4tgk8jjxdr249dsndyg9mwz7nzaa4rjch2nkf0v9hjzypalg7vhl0ylypgjgxq80x7nhvx276h2y26huzuve9z2twt29t2eu5pv53tzcy05c4kdwmevsmk9yqwwd75v2ynkct84gypntu7rrqj5ryneq2g063dampjfctrrw48rusmeqhng6kkrmve9s6hrsk0rfn8p5k33wf9ursy3c68auqq8n5rg3lh83lx3z6kf0ncqxw3dquxugg4a3yj5ay7u0dfulxhux074xsa9nqa3nwzj5lx54w9sx2dzh73leqhrlgmr8qaadvdqedxxwaup6p5l96e70n6e3w92lfhdhap77ew8nw6skutptclnawp7wxgkmzhry5sg6hepylg7p78aa9e7y8q4jf7twdzexw2yh9usdpp4k4gj4ffymzymk223q72h27ud3afq79jf3cue740zu5xw62l3jdsxg0de80lca333v8mpavnfyvtxhv3na95d47t9j846uhwz043hkm2h35l73eg8aztkr0cylnvtz0l7046jlm0uxyr0dsv327zz2l6xq7cwa6hnpdyn6n4npjv3zky2fz29w7vk2kz6l62cpwwkkxll2fugapujzgce5dxeqsta32v28s5323tu8h9wcnehpwpxcmw53kd9qz0fdpgsnqrxsf9nx0fz7304u6frpy5xh8l2re44hcv867y5zvx70f669csdzazw40slgwzuc6fl0h6mj3vqtvl7nkct0qjt52hjg3e3jn4lws3hmhk5ttpckzht0y498qwq46wy745vm3wn4lwwsljk4twpq3q4ensqa4hu8kzyadv0wsjvyam0ud", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1r076faqyvjj3z02v7j9456u55g5xycwajc87k9pm6t8tfpg0jjvx32p6hsqpl6yyt20uzlj4lpyw7zdephmmfdw2k7qtawdq4g7n9n0pc9epxceweres0ct49muc0548ayq5msl96mvxxhfpnwhsg6dq7q9n9zsmj54px03j8k7k6fdyysrrkvj6kx95xcsrcp3ryr2ppz9edqemxwvgmph3unyhrpqzd6yeksw4c8lslyzys7wqezulslkn2vcpu5nxyvzp5ncadjg7em8va72mdydl0mdzf8e49l869sncmwxvmwu6v04dmf608cnlpxq64mpfj4ktrfhzytneq4r7kn77w6p9um8mxwuepvtntpalplehtpzgs82qj3y2vytu6tmghma2yrull0hfxd8ufngnfejhetfe9myjpatgaz5y6gplecqk9d3ghvnk3v6akq92tlg955y84594jh6r3had84u0l7mpe78l5xy0jr27w8qjpfurkqwv96z6cu2np5hw4adv3zyc5n3qkdtfxpnch89s9tzmv3ru3xt5jkc2xlja6zvpt8d8h2gtp29pf49wqx94u5j5aszulw50ua98u4d6gv6jsmv70q9l8jqjnx96mf6jw63qvw2rq5l7qkvhqrgv9ufvm6lvqhsrs3en5hj5elwjzdjwh833m4me6z4jle8l5ksf70p04ezw3y3pa5rx2dy8zkfdyy5dwn9dyvmrvsgqdtqryxwfqfdlf8w0g5hcxqepcd8wdx9lgqks4z79s9hlfz5xfyujy7retlpyyceznvsf6vc3l08k73recxea0l0ufqxeyx763ah35eudmkd6fy2gy2p07kfwtax83vntwgnfyvp8z5fa2e6tysqewfnwjva4lyfcnhnv0jxp7h2n9qj9h8dcumtvusz46d243y8cwwf875jatsjxkkqxrg7t398gm4czcwlgcfcy50vcwucm49hqrhujykpk2aje6j4ja40st0h3zahj7qqc9cew9vfk32479f673ql2cqdnn52ys0w8x9pcps82hmqncmyzkzgsx2yk4zdvypg76n59jpajr60tshr74gr2rrl0wdasm9qx2zfjfhn6ma5fyn6cch0h6cfh72cfwrqzh7j84ed0q2l8llf9t65ecr00egepevv7zypekugfcx3fvrr7syfj0h65adcx3sljye4jaux9xz2wh2lr056qmmqdspg9ujeut4fsagy9ufejpmusaycvwvc0vhd4pdfh8ps0yy34x3chfj0ajcyg6y8ccsuqet8emysqhkvyz9y3w0v4s4zjznexe7u3yflretz6jcgaeeng3qhy6gughhms8ncedpt30ew9yamrfap28sy2lephvm2wgau5memr6ese3vnslqfd7qjaafxsldl5der772yw8nm33pepgpa43rz28ghr7pz3ptm4hfvdk0u0rr9ecsprmpygmjkxsapmuxa3h2zgq0xpwetjxa7j6su8asdeqytre5k6hwhtmfnvt85q435zf48ucl6xryfj086j8y9zvw48hh52f9u4gtq80xk7pk59t0xcwualjcvud9xgyv7rda6zvv60catjkf4px6k9yuysf0etyhlrwtykgzszhlzzmsjracvmrfru6d78jdnhe7aj2ukvglkexcxt75dwcczkd4sw9xae4mdnrknuj2tudth8lkpa7vz523t4sn8w3feyt8q7f5esfkrrnyc4mqj80sysx8c69wh630xs4d2a6qrhvarf7kkykf5ner2syhgkg4fm58k6mq2jxc46jmx02448fz3e26h4ughe87mt67qy99dsrech3y7cyg73x6dx6u4l8p205ecnpwv6kt2yu0xm4xzvz3frplam5yvd2h53wutzqtf3dy5e8uyuujce4e5xsd5sm3vl9w8uy8rwm2f83x2kvgq3kwv2kyrj4lsesgdrqfmp3c2a67v3gxxp57xdnr3rfckr9vv6lzpn6fqedr58zzh077aaesaf08stw9mkyr5rsrtlf0h9kvgvgdpyn7asye0wtcw4tt86l263qcmwmq2jcp9r3hy5df732vj8a63lkrx9h082eaqhllheh3wde4rdfkq3ma0997nwuaf8ehtx37l46uzadgdwv2jpkfzv7qmvgha7a20jt5sqj04ucy0nkh9gud3g5z9gvuzdtmuezwpqzjd0avfj9epvxz5836cvuj9xdkhatmlxpuxa8gene7zkpclunnszdakqycw6z4rcu0ky78e4nghnhgnksuyye0ffa872dvch823a0pqvz5edcvt2z04skj949wy86ndu3vhs7tprtu27seycuert8h0jlkvr6dc23c4tu8j570760k9ff20he3lpypau9wwxu6l34g0rex66zngp2t0h3uk7eeeal3uvceyp8guswcuhtazycemgk7ga8q9hmcsayar70rnn73t9fwjj95lxv7zjga33vvpl75w3e6gppxjvy8ye287rv9jdxkxy4ua8fk2rc4gc53x0yqsfrz03qp02wtmlmjz8w4hm828utcud6ljmgzr62fceclxdzu9x5yefrpvnq7nzl9fms5rcetu3an9lylr8dzjfjnpl03zr8jl9acssfrrf265ltanlaya6kvgm042h88ylru388cdfw6yp2lxyau9prn0fk853fgy0506xtlme2fu34a2qz2r8qnwgp9jd8n7kyex0vx74glj0t5vfkcqpsm44e2yrzxr68w43z0lnqw07vthtcwzakp6yj02sclg4a64qfm69wzz84kea0k60wej6czxw3kn57tku24ln33mfql65rgw47657hfm4h5xdpx9kxet5z6l5pj09qjtj968stwfwztgm3xkek0aglv82mjjpcnnmvhvcgqxemcgywaqr4aca9u9xhuamsusjtpqv3zx5fzr4gud8rhghdjnjsant3pw6nn0l7l8dz220tz3a5ynflc8pvleq2ef326qt9p5gd59dwqv6y4zv4wjghgx6m4jjn2k3smcmk8p0ccf3j6697dg6r5nh26l7jax69pk2z288tcg0e4mud4xy902wuqfymhu2yrdvnya759h6qrykfjlvq9q3wnmfhekk2r7swautcwx0ku84a4f8fqcdgmev083mqmcv5q7psu7sdtd9ghjlqje6asxqeh770u8rqkwn0m374mwd29tgr0a5uv0qals5xe4mx2ujt8x5ukq3hyk7x2k2yzv60848jaas2jff7ahauls729xgs5z4uf60884tmftyr4dff3vd6c9960fsxnrqleh6mdm4vkzclpg2d2tk3fgmpw5zw6jayzmpym5fhkl00lr82wc9cz7mr74r5u99me9ggcupc3z5j474f873nlm94sfshvtzjhpsp9lfktss88lp3e7xu8wu5sxqfzyp2n3z2k4", Network::Main).unwrap(), NativeCurrencyAmount::coins(125)),
            (ReceivingAddress::from_bech32m("nolgam177ntzq622gftdhfvmclxcygwdgg7e6kplvw6w956xd6ljx23aj8y4dg9w53a3m3502mq28kqvecy9dd2chc5nxlf3avazz2fq4fqga3wmm8p0cfwqtes0tp63npvlgxyq32m3yldts7yljw9xjqtzvac6r2me5nft238u8shgs23q4twg888wfrhzxnqd50f57897z4n9xasad6vk8l7dzal2280n7w64m3s7rtnn257rqdv5zf7epsenjrflntjy5hf9m09r7ng0nmxmpmlkkaae2kxqgecm0em4dlhd3ru8tzpnrcmq4n4njdr6sq3nzc67jzhry3p493m9r9l0usl227auy24ay2x4xasgte24klxk2rl0ayuu824hvp66f3wjhrned4vp2438gjhxslsg582wqlncq7yvvr7juhku54aueg4nq8d43s03v28classex927udj0c2r6me585n4xzr7yc6enjpypcnuxhmw8z90z0qua8h8284hpcypyjyka8scz4n2qask7wxnwylrscwhnllp2ld6yn9w42fawvy00nnpkstg42k4hpl5kmpsag6mvxp78f3w5w4x8ywxrcsgnx74cqgf2rf2hyv8w7vax2peay0jzncj2zkzk4tyx0u60wxtq9tswyer9rkjmcvh4mm2djtmg829lpe5zvg55mgwj03hv0z6t0j6vv6hvrul78aq6dnj5alhpn3psp6zvxgfusx6qzh9kctpx9mzffw2cd2p463pam4fxrfsm70wf90dwzw4yx73x3ukk6zp7amypn4df0nwx9gpyc0r4p7acdqcf2fnmualc6fk0lzm98he24al08gsmsc6wdeff079xtju766crcm7dg8wkq8urpxznaxnyreml0zs5t0l5k4rx7fjh0nvc08kh7xuzxpkmrvd60heyufru0z5ek3mngqvw39zz3vuse4yjsmypsjlutj30a29vc328csrmflpxwyqwgr237rgt7dv2gej9edxmcr902qtyjqfmjrln3ev8tuyeaczqfrvxr33707y0xuyy6gu4q2vgxv3y9vqczek0l62fhrz4ffwtdua9eryuen4340vgsuesaagg0f90rg2phxnhz755pw4w207ma2uzasyg2ld39qx7w0kpkyz28djqgl9gwppcflua2a36mu3a5stzn0vqp3qs522qwvkvluq847xtdfsc5nqxj2uvcvsc65a4yquvn5pula4fkpc5dsurv9pysgq8ak8ay7uctv32lnpztngnsawl7s98fp2ketz0mwgtl29pxlau4l470mm828rah5salmyhnms92hw53lx2cmckxd897ympw7ct59td6z98g0ed89wd65kxutcy9pa5yh28xxkew6vpemxxx6eegw5vskdfca4ff7yg4urnwspd5enxkatlnfnxspx37xklc465sv4pezyrgzppffe9uj9kd9sjyvkjw0d0lvsflf7dh2f7j5men8s4rf5khmwp4jlur6r0562fhjrautvf2qhsquqfhsurdwe4p5ppt6s873j78sftljxzg3rewnt5phvgkc7ckkcen7ctvn8ehrnn7x4v9avqy9v2dh7k253563funqgp72m8ya2vy9nt5qx4ax5e8e7d0qjl5z7sgsr2202fn3vmqlfzup5h46lq3wpqu7yje3yjuhej3dqxuuvdyjq55tm2kcwused6a2r6cthntwun364pkv8pahdqukkdu7hcc54laftvvtk5agevc7ekn8wx2zcl889mq80wp533zxrek8zlmsr34fqlu49ntlw99ae8turhcc2za8v6yglf7ahd2fmydq2qhr2smv94da9wpquyaa672tcyaad39cac874vca0a7vqzqh75z0mfz90qym8zqtdxjv4hgkuh2t7x4khezfv9lca3gamdwedylzmpxw24g2dsyw4e7nc6rc05e4crdxu32xv750fzdrldqvqf57kmu0ggtp9uxz8xrwgq9nyw00rc6eqp7v9t0y2mqd23nqp8c48yjtehr0d0l5cza8nszxn7y8m6me9rtqgn28hhmphkepaf5d5d8gtk5pf56a7awlw4t72mvf8qqu40qc3xdpeg4csty74azk6uw7l0d9k3j8p5g5ev5naszle3dwkcmdvx6su6a8595fuxycyv80mvtazde0cssezhv0x085u3l577f56xhuzyysy4aeyvywap409ej86nxagv2ph8gkvcwuf298hz5qdlunyk0ggpuw9h4avutnx7t0gd3dx583g6syq67g3jxe77lfeaxx3q878gknkh0yz44y77ukduxcljslkvkfay99j7vvq4k4860ep2uyq0r2fpl0qjtlj5dnkpuz0uyu4emhgle7qccyqefc6jwqx4rwat6zxgftl4ukgpqr06h6rgz225sw57whlg6mmpxe604qfjxg3qx0wty9ac628s0yfetnge5uhqlyd7lva4qu6h8r88ydhu5cdg8audt8ctk0c5l7letmmgdlxu3qhw34dnm3r0vfxgh60kzemgcdkcd7duq87apka4sh989meersqx60ha05j0crleawnvgazl6l24a63tdrrt6lyqppdg55pmm7can7dxf4hs26c3skqlqnxk5vhsd4chnzs4yt8g2qgx0nnveeefx5zen3slgz7mtrp38g459l6qdj67lcm3emeuvjxly9sldgyu8ns6rszjulptramfj9d5wqnh3jpcqg2tl5m3aqugr7thpd89nkepemkp96llckdf4ndyz56390srsdsfz90c9me68pvfutdah3qc2gassjvhq8wkdpd6e8nqpf02rzccj08zuzmezx8yugx4c84753gkzy5sqnpjjtqhwvtztlf7qhf47x7vuf7sjv85tk7a9reced6szezq80zwx3j68smetjh6g8ua7pljmqj3una7jkfdlakxh8vtu3wctqleg2gfeq7ffff9sdvqmpn24uhlevmrp95f0nhj803c90qlwkxegqxdm0h6m0mw00yswwjvyeflr86ylcwecjs7s6j2lmva63rz3q5ygtnu0mm7gll5nhkznr6zlskpc3rvudr9crug6j9d50eu3yqy566c6jv360y8jw537tk3tvx8w5kg4lcsl8lr0j2lk0x77v2zt3pzu9wgc260fg3aw8mg09jx5e00t49sp4964n935kgtgmhqcne2egdwvcqwq4lmfcdrfchdswrrhr5nqp8e9ydw20ztcacsx6k3w94263c5x2esg9u0k4t46jffsx6cn4qh6lyffxug2rwuva4dpnwq7t4tvyzy89wzx660nt4nju445wcagj5twf6xjxejtsnwaxu34tfx6f83uq8gczprhpwtzr92ef59duln9dv4u7l8zxdayx7dz4f6vxtzrwkj5w5gyy2l4jwjxa2kt", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1cqcptrs795akt502dl7kxk78ceg7j3caw3j68c3gpdyx8hsmdgrx97r6mwpu2h62rw3jh7xhx9qwq88ezrgk6yc2m6laq8fvm3gsd38q2m0dwhvlwpc6t03q7j95uyxp47dyrvc4m6fr0u22e9q7yk3wf6y50xvgwtrmdm57ve84d7gqjund9qwdj2a8qxhm6t30dh7g496vvvdf2n7jmmtp9vsn9ycec0yxevqae65h40zt2ttew4y9gmxatdykdgd2wvzwrj9untnaxp5hxzl6wyuamqw5zy8m6xrupm38vqerngwuvvyjqw6h3utydug7crutc04crzamlw2qa2jc0ky9d425fznjjm6t3tf3d4tynz8lmg5jrjsj7a03ah2ps2stxwesj4ltdlshp635sqd0876g64lgy8ze9pukq64rxqwgzw6p8sglx9r55lg3lp2a8k6pc7vqghxexfkw0x6xz77vdwdymlsjnwd6zyta3nsjthdymnyqvm4fcr655tr6cmx8clnh6jkxl4rlzetfz30jgdmeslmjdhds4rq0t48c85xsg66cnm4a98eceu9z9jwnyz5903uy0u3yk3rmdxc0g0medyx45sdqljk6dwx65a2jxx6uvnlarpjdp6y9arc66auxge0853e9y4d7t0cvpc63l8u5tczm72wsqnymm4d7g8f2r3z5cjjcmxpasuh3cq4e2snmpn4p3cfuwuht70t8fnxmm7a8rhm3sd8zy4hrfpkugljn3wtu4pgwn69ndy0vcrrxwqwrcxm90vwzr2yk0txe77tfhj6pvx6ey3l3v9u2l2kd4yftxjps03jgn7ea9v7qqlu4afs88yem2fjlcjj7dppe65lqvvwvpuyny43d3tg605809zz60aqh30dqdlsgsvpgw9futm9j7m34n3zwcpdmyemnqzv5vw2dhddtqxvf3qujjafan38uzx67le6mzmxtvwnkfp9vy5q9j7axagr7zym8qnae6wnlkg0cfuwrqd09eqctp7vxu2vc4mlczu5pj2v7gunzq975ek4h7wdhxa4lcmdg9etaj08kp2ggvmq6a833akmd2pgnyvxfc7hmp6qnm8hl6sz80xh3djzqqpc0yj3zxzu4t033ywe9h04s0ju6trzdluecnhqg3zf58lnqf4ye69g30upag339ugu67gv7r23r6m2edlm5u4n7mcys8k8qheqt2jm36uktteaxa5pwdl6f2v2m43f7wv4c57mdmtav540rj5glxwva4tz4w03zy2nt47q65nsl2gmlk00ys8kzz0fccup0ekrsqwvdndqppnuxy87z7ag2mre3fph4mpzal6esd993jx54xhu0lx0xjqlldzl2u40g7h2wej9cqnwn5end33a4n967z4s89trch75ex2ra23rt2734q0yjtuvj7cwhtlkpzjhrz9g30v45z28lfwax4t9el88y94h426dtu0lp5r5zagdn3lpvefrq64cahmqgh0h3ha0jgdadwjvt35gce3lthd9uyzt9284c8sz7v6m2q6jd5qmukdk652fffyn0m9tmn4844jjz6vx3dmw3dcdaqlv32gwtx257qlfv6le2cjjdmthjfavwk9ah94ce805lhye5l5celp4ysnqnvqfyl6s924f2prs88yvqw4rpp95kldfnvfhv37fq4deeu4yz8faj5glx6flt6jn6hk0fsufv0adsnd6u6nrcl2ad4x5z3j046y4fuwxw5cqks83x4k2rm0gwtn82kp9upu7cvlddsvxuyg86wfhzegzh9vj30ta7wglk2sa8xwyfalez0cpgewrfshmw9x40k3z6r2w9ecqxve7kk037msw6kqt0ytacgj4hpft7h7lecgauangsn6hjyxzaez8ptupdqg9v26xm0ps2aylhhxxdcagd50geccewaude6pzd5x0ls5aae9xm7pytatl9preqehy6y55gyx7hne6sz8pq2xujf3cse2rcm4a9yhk56pvwug2us5v9yrd3wm35rgcy5at2zh8vx93xeutmkjd20h8z7djyypzyw2sg4wt38xngsy62g36yn82shff4fr6nemm9vdmzhjqrg0v4un30qnwdu5z7k3twxm8nkur4a7g48udju4r8e0mnjuunw2gq6kjfrxz9xaadsjkmh9j8z8jda0s0ncwflfkhmm4fyfmyx5ckmxdl8lyveyda79jwrepqdu7hfanmcrjade5mcr9mlxdylhent9966nd678zs40d5kznujea2a62y0y3hafd5d7wajlgtdxqklv4jlwsw82c0u6enpkzxawcgd3pwlwnr9a8cvez00xuggtk27lfs9wp77u4a78thvmk2r5p6cra98yzumlpcdcy3445caw3ypnsedz9ttmpyl3ysn08n3srp2xyqj63y763kvlnehce5an9h4uscp9dg4kgthphy4rttkfkjesuy9p2002df3x54pcy88np5s4w4ryhtdjhd42fpav952ngh5hhs9lrxjnad86atr525f0n7msy9wn8avy9cm3h0gqn4cd7gr9phkchymp7cdc9k634a76gr462seu8s9ks3ukvv6wpkj93tqee2l090478mf2qma8gy4ag27rrfgel4t88je2azqaw9pyzel3nm0p3tqdd5up6729uqq9knqtl2aa0epfkg2tcranfwejkaa4pwp623kvsmn2x68d9xkw47975jjtkg4ewzjgjvrcnjtceu7qnf464kg0ft6ymuxq993rwld5l4k7dfee4htfewkxl27pyfzpwvur9gkf6z0kejd2y8sjtlkwff766nj4u8mwdfup0kgt3sxj9txmq3hswqh6eu03gr0dgkgf44k6k07mrh4xez3y49umg60jp8afvhlh7ah8699f66anlaz0exf9fymw0afp7t55nkdwmp6y8mhkwl2d80kdue7l4mvrkcxqqu7r2u50gj67z4rr80kx8z38zck0wwrhsgnsp0aulpuxq0uuvr8la5ej4pwge2azg0el5vjgf7a8p907kkvwnw3390p9cjqehwh69jld4j5tr0zzwu0ehlwddjrkc3sjn42hpf65hn8vg9gnp8mfpc6eckcr5kfwreduyzul8xkj8q9y57x4n6l8rgsgerlukty5fqrs5txw75hzmaydpnj7d47ez804q22u05ps360nlvstsre6cksaf32k90ven2a3h8amh00v4nvx7ua45e59rj0fjmlr22vxh4s9apkj6a5glr4yawlvp4pn9f37t55nqcecnpj8gdejaq9hqyyq2nn4pls5yzum8sfhclz5l0h8r52shtq7azhc6w4kr6fqhwcdmkel787pqgggdwdy85pxdnc85al7d7sx7wv9e46882rsa66c5r8x4thycr5r8tf65mk04tn92aqypds46", Network::Main).unwrap(), NativeCurrencyAmount::coins(442)),
            (ReceivingAddress::from_bech32m("nolgam17dzrw5lmxmnzkxeyw5ncd8wa0vtr9vjeu6c9jq79qgsyfehvqltrz9t4255s896ucy3gfv7hmtd2tjyuwlzneel7werl8u6jyrs6zelahkk9t84nqrrty5jmevmntx0mmka59ezcdvtcg9ufkkswyc3ef7m3qfyyfpfetsuczf30a2dgzq9dpggcag4zyn7gkgxw6z2ktxjm6twrfpfmqttgplx67me460j7frxlzwxrqzqk0hrlyrggrpqs5gt4j998wkdl83c8zl89xu3tjemfe9403h57w4w4vcluc7p0x3gr0fu43njlz35lp2e8vhud9dlgfn0nfrvacca6m7ahdrw9tsu9tqspghteal99tju05l5aygcjquh5s0jg6ctcvgz4dr4mrr4fmfwhjxgjx49gumjkqvh7dvwhz9ymjuxe8w4nrrvhymgs74tanfmg2r2z8gj2jtmrxjdpca5xvpdafxrtv3tsrmam0vm3hhfrtammt95ataxagu6x0esa7rkc8zzgyz4faavqkmnngt2euz9yustnh2p9jkrvsyf6ufl2vl3ttlmjl44lcpwvfaln2fpmz532cwsjy5eem0ssqgvulj7uultt9n8e8vq0hhdr3zntrvc24qth9vf532sthu0jk3hfavdjzrx2hem65fustkhrn6wnfpc0m93804jf47lx3faunews3367lydq3du4rtmp6zyjlj3vh363xdpaseny7wzzh4w0u4zcnwuged37j95hafvs6xmhe02khrvp0u4q6z6xh3x53s6a3sruk80385vv3u0chcnfwxka47v6gacwz8js8h089rdzgy9828k5m5e779aae29pr739s5wek8hpnauz9swmc5p4k8ca95em6r8wpxkp75fxx0e3xrlp988plh4wz4vvd9qz35a3usfejyxchx2xwse9zvat6yv66tmtz3mh4ql6djpzfqzus4e4mjpv35mp4jnx4sm23xpqtauaae0amvff0hh3h2n0f95sle9vnq50qwzytv5um9h06zy8md3efpl2ht2klzua8a5w6v66eusk92ac2g9ef9f3khvkhpwtg70xyv7jcqrpkednw0yxp2x99nj645hes4avqtgc0dkrazcpxxt33yn02qcmt5u644r90ddrt7g5hepeqhknfggcd0c3utje64frk8pesg6fzvu2cxz3dma2e3g87c26a6cw287apymj4e0wmz0dps9zk4c0cqlqmgjkates4r7xw7hch2cc4gy9w9k6ghqtq9xxyxj5qdj263gc0z90ajj4parl8dwl67nd6k2vs98vgchfe8qusqvlelm5lthk4l58u9qjqfwklkrpmqs80l9897cr4rjq9vk4j6lekmnkdq97ktu9rh2ezc7gekl5prgscuhqeuse9wqx3r73q2x05kuwycdkhnmjr0hxnpm5943yrtk7jlxxu9y544k96jp9pz8a98x9xnjdr2fxqk25n3ppvjzvasuzs4feav5k5vyedrlshm7wvs2p4dk3c5y9le30tra562ld9qh6gknwngl5hmvj8vw0fv0jntv8w257jk58ltjzfshhypl7779vzrvsfwv6esnl69tl6vekwdne4twt893yfukt0shrze0er9ea2dtgrg37l477khvh7ln9hjyx5mgrkqwzzj3c520ldkntagmmzuq9zh0h97rvf995d4yssrse5vdjuzuhl35d6ywaypc86m30erera0syz4rjhfhugug2u8d0k6v92qhpucyfa4hgxflhdxakcwt8sh8k0uvgulqa6u70ycl5acllqe9nz8pvfazl988ese9fmsv5nglzek4xqkmmfkstqj22z2v0z2dxyuedqrne5wgwyclurrswx55nqx2mwfs68lfu5d0rd35gd7a5upp45ml6k5tr4lfyt066x7flt8y9h704tpy8k2ppppxj5g92qyf3ndeuxqnhktky9v075h49rey0kld9vkdnw0kpgshxgk2s5dpdcenf9658d9esujvylx3akmrs3xdseu7fu3mgush2aka0s7sy49kvcz5vef96nuk80qlmz28nf8xvk60xauuenegp9rrezdw3ejhpax5dpl5s5ku83dlt6w9rjtjcc9g5lx5crazzxhpmflvckxce0mz78xrkg3cq2dwg3zt64q7msqlthjn887vhy2vlsq9u55n6u068kyt7ashxjvm27pm0y7z52jjdpkyd6c39vvqcuqjzspqjlq0h37k2tmfvtywt8wcqmal3s4qdl3mnmwlxq43qz5gj9v7afch5a6u8nttw269czy674r52djy0hw2653ng6prku49xhrdurpy3nacfcr0pax2cwq360z3njqhse6udrgaedrt45ecpk5x60j6v78rcp3yztrz9zcavdmw0xmfl6lm22hcmdtu74gtam93rav4regqwaz366ezq3jahsq0zm9n3ja8c780cy5ptxjluvv70m3rhyqkcdlf3wrnee4z7jehear78yz88llkzah48fr5lu4fjwycpkd0w2kxtc9pf687mhszv5ye00kgp3v6r0tchped0fcsapkvx0vy5zqka7nzp6n9pa94swvrm0df794ekvltkmshufutyh2daeltvg8zet63j8gadf9vye2ah5djad0fu62d9xcu9drgxsxjsgkcsl98zqfdmnr83jqhwgz9yua6e2h9m0h2artq6p9n2scsed7u2takplddfrad59z57k475qaf49whth9xdlatrqjz7g9prpzs6dv4t65dugg5ms0vh3ce4y3gfugh6m7kl87sd6pg3v0pwywxy4432ufkfpc6k9x92lkd5ajewux8u3whpv6wuu4vwen2ry0jd8re9ryvhq8rk5manpc4efwv2xdyekncsygdtpnj4j796zrrlsvg3jt4yu9ee6kq36prdhzwqm6are2fgly3wy8x5sflv7xd43t5s66sc9wsmjts5p49encha8dl75df2v7annjafjxw8qnehdvkdsnqg8nqkc5q06usurmgw64cu4szprncgd36n9yy9zjz855yv579uyv54c6s55vnxtedpkcxu2gyljpxe2t7j8l6stdgq5k2tgywkmnv9t9f8un3egejdw23j7aqjju6305eyg0skpt0663fyrqp3vupqjjd26wkyt7ywfcuk768ujaclywhus44jj7m7agyy5vd7ennurhaexnplecrdpv6yfqsvyztf44wqf5pe0hmyc2ljhcj97elnjmwt4vxm8l279fqcnmqq6w3klfs5nd5dcd0d4zh2emrk3ct75z9e0w4t8fdd2nhpx5n4c2zdvdtsv5x0vkvtzvy9mrygzd9lekcqn5yx5rpwj40a3mjcf8l0gqq7tvzswn7pgxm2qy2e6ny2vmr95hl0p85nmqefwt9wzxyskr6", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1h6s63ypa9nj6p452yd7xv8gssmn69jege5us5nwujpwh5ta6n7ynkzxamj2g9t320jrupls8mt9axtzzp29j2as4p4tyaagheml8a0tjxl64kulypkaj5rz4swjljutrk8f8ph4ghfr8mpjweelt9p2wtvtkt29583hf447xa5jmkzfqr0jxuhwz228fw0p7k9l8208pe34dzyq9vlx0zq5cu5tm500djzldummj6dd4s6r2w3lahzfevcdmg35g350q3qd5lp37pcuxn493lkkzxsqsattv0qrkd6mec3wrfc23fv7fth8uk948fqxt4gwatsdhxe7vy3zn2ld5jjshjvqpjfxrajclz3d6hrcgxh0969ljhv4sgmqpvgaldaahqgzj0r2hmzkqp00x5e2hcelutv9dh30kfnl6umjzmr5nuwtmdrlvqhk0a27lvq8maca0phc5l4ajh63ax0vdvt9hgea06nuj2lxfxsawuxu799agl7u87mgmtehjq6x6rnqctm9kwz2zrvpasyuf4s8u5yxjtgf2f8htpqfsdy3c8g3sv4av2xkgsh07fge5hszg44rwajwx78w4sr9pzt85jdp3af9q3jx2ptlahln97jngg6dr8alf2h25ju8y93defn7z285d5jw2g0tdefqt38rjprclegttqkx4r9rxyz3yvjgwjp7dk2yjldqfzx56g26mehxakz2udmk9ewgkw72dclylvdjx0w6sshdsx0mwpx8qq42jd78zcafrdg7zpglprdnvce72qqeu8wdymsj564cmatajhzgx3cqc7e6ht6fn48ktc5sepz9ztx534ka0n649z88re2nv8u6pum5u8k93xh3c24fac05t3g6j93rhpvexc8qeykvrla8hw4jrr4n050l2mg58xseukyhlxvrf6rnzt9ac503tcg0l4cs0av2ek7su3vy0jfcgmm29mv49nqrkg27w5xjc9h7hqvnqu75en9zwldnm3g9cgv5e6r65xj48hp596lnhhajmwktj5ts74lmct2jjc4cfs7ml5y4ly88tc2gtwau7d2gd6jmnv763p0l6mm97ugw6g03zcy99rlmejp362z8ghv9dq3wzgtl3xx2m6tc762a3j2kfp9dvcw4l9jpvxp4gx3m2cxdf0e2vmtp0lx7dpdtn039wzgt8gh2xk6jslj8zvlawtsf77vup2duvltt8ymyntf29ua8ct4974ld8qdddz5tnst8d5mhasrj53je72h238t0q5z2srfnjswtmxngxzu0u5sp2nkdkyvuuj4u6ru3qtvxdvssjkdeg6wsetd7n9dcaas298yjjqc3ua66fkf32zdttdussfhkxtrklun6tk3rwth0ygapyz8ay4n3rae4vx62v8a98zhf90lrf3fswnn44e022hmjrq9ejp4w558ljz0yypfg2zvexknw70842l758cgs9fknyzhtvhw9z0eyphtzh4zh0taj57rn9gljqn474sxg7h5klclkxyxmlje2c485avlt0zqgplvkafjdwpr6s35fslsyuvpne9akr5nzcu0ns2r93zn85fevxmqvghjkczdsn7h00selx6uz2fdtqn2x4gdrsg6wup9sxplxwkaxurkg955zrx2l6ag5jc4u3tdk4ycu89qdnrsmcg2el0zvyzxayqre8dlxqsxhyx6k69wlu7ge6vmtqcyjws82mr6z3plnsj9zearrph6529kzckrkudlkg9g2naycr94f5q6y3ze8zg285vs8swgrkcjrf9we600mqq5sjc2uwmcn5tcmdhj3txsywwmuqurv2rkgh8v27tx9ap6ljrj8gvjanw9ke65pwpuce9tkskd5sa20lkyj3zhnzlcp3uadwh2yd0kjryjnx7uc5s7yrpc2ksxvlysqlsn5nd5e20na63f4rqra7qdjq4n89wjplshsl0zysft847lw7x463y5lw40y3ntqu2eztaczmpafj3a08fx9wdm7gkcphkl9hxmfx38atvx9dslhspw3s0crjm5pn9h9hcwphkenu3e76a6450hz0f00g2fy4gh6t2d5kwzxtp652vz432avx6x5cjlzvry2l5exn5rpc96efcgxqk270qa4q824wrgcjupxkda4x58ekw6gkl26l5kecvk3detxhay5qtlsj25xf0dkakmnjcmem9thq4dgfqe92c232cmvgvxus56jwexhn66mem43230hvdz2eglwj9n89rgh5egzt5g93edsteqsju4uu8e7ts4uw4wdzx8kyrxwju8ztmgghj36h5zyt2a3cc9zz89d429w5nwjrwcagcz4eu62f0ara7avll780jwtrthtym6ml2k5rxg6409up3c4zd9r8ssfaps0fh4ecvfjghu6fwas0gtuc4x34lk8n4a8n8kgwskflxuj3na6sy9q039zusm00phl4pktgmyx79sxa4wwvpqqr60mdjlazhqzq09nh8wmjyf5prmhr0r4he9ghz008st0p74dj6hhz7nhqen6n3w5w34aect22ujyh5lv7pdkzqfkpg9eq9thh45cdfk4svhnl7nxnmkdqmkpdn3w52jew44xy2cwa3j0eq0dw7quhzg0l58dz66tggsyncmy4czrzht0rnam69w5482qlfflpm03ypfqh4ac0slje6cv6fzkurw80haq7qklyvf83de0p7l2ekwy0p29vedrw05zsqw2h9mwzs3mrmdy5lgqh6j3hjhnpnjs8juamfug6jpn9065ctvjw9drh9h3jf7dtdj4zcew7cd4mwf96r2seustjgrdepkqkn8pnphpv6r647gs556xqf3qsjherhsmnh0fl6fdp2yn3sjec4v7s7h9f4sk509us84ss6cdk62s0078v95qsv8ul4y96xejvuyrjdq63qeljjc5c0sk4eu42kn9ljxa93s6lrfffq6v9yyjq04purac8qr3cpnec2ledv929jyk0s9dft7358n5f3fpy2a2z7g2wd3usqt2yvkld6wky4ntxvqwtjs7nslvp8556t63eqskvalhnjgxaydl2xx4xuhk3ee4sfyet72wthd3n8zh806j7hu8ffv3s7fj85dzugrlutpwsayamdden0ls0adt9wt7vkdcrtyj73rujflej8zp59hja4f6qemw3lqrg76uf4hxrqs79zn6dyf7c93vzua679cgd9px55csqytdssu8rgnhhwjdxy0l4ks99wujy7gezez9umtmtkr8djlfgq7durkjy8nmkpgqrkhu9g0cyewvld50uhacl7j9jq32c3h2jn8k6yutd5rfhfcas7hdmzuekxn072zx6hpc2a0wumpvhnjpq6vnj9emdt6h8a7x4zs62sgsuq2yvl08zrzha703xw3phjdj8u267rljzv335aqjg6s43mat", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1y00339m0n5jtct47e8nsuqsmnydas0vnjqge8l5pk7nfz5hexlnl6g04pc0n8sz92mm2vvnqz90kjkjer7d7ccdzx42qjg9r7t3a3ngxu0p3rhe3wku0svw64uayacv28w7ugg7lps5xqgqyskmqv4v70mq2cqt74wrug03azpjqu0qf8lmc8rsvlpxnn7xqahygkda2wwesf5nswkayaqskd67pdfr2sj6g6va8zqqr8xs84nhnh9pv5vdcm553t7gxyxug3k9r4pr04r8r880n78qgsyh9py0tzdc5pax6e6wtsyfmngzfdz599q6rw88hfwy9eu5v5ky59a3y6dx3xxaxcdl5aa6k38qr4rdsdfu4uvpghdtr4krys5d8exmuddtrcjz04fuhhpepm2a39ed4ftuxphx3qmvnpdef7edr75m7a4g0n63vz00ull9q2jqf463wzthls325gskdqlxqntq6f3tdnppn2z9kmfpqvmp2hy8tsqt00keu9kv9na43n8v8d5cykp6k693qxkaf67ewkhdw7w45nslva09zxu2png4dw5989lvrg5krtry63cm7pk2kwdxucpute3a94fg9chwvr9l8l2ju2y6cf2ywwrk76h4ajjy6t05yyumz252gtuntvxm4lqlwu9zmnpmcmqpzymjpw87se8h5y8fhv58e2pxmh0ez00g5f9p3qm8x052twm37h6zaxc7e5zxyks0z9tflglk40ams5h005q03vae555wh8eeeewh0pcs4c32udv2l90vcmtqzd5k0226k56xdm4rltdql0qmrxle9catuvekj65k5q34vv4u9465adcukt853f5ltszkfslnx2jdttv9nypax575efc4qwfr8y03328dxp9cxu9rkdpm9d8490tj3pm4750yuf8m5ww3amz6f96v6nzmw6cc6ug8njxhuf0qe502qh2q2sgwhm6fmrfaavzenje4mmekq42uzttc4tg4vj69sg57utae7hzlcqv7msydlsw0plg7pmkcrnunzv55t2km3shcsys0x9ktx2cjkqmdc3gf6hxmjk0v4qmfyt5jy7t0cdv6608pv72hzq3dcyh0jkcu7td5g7se7fv2y4xnwrp74c47vkrfga8zwwuxf0yt3y6nmdwmlcacgqaljev6eqw2lej2h7srlnlaxnvrardmuz27kfs0876lazhlwfmvw55djjlq026mflwh5z8aldwj6q26ghuealce8veg6wtm9kyty72eakplqx4a989hjt396hepxqlhg9x50rx9xex99d4na4cna69xj38y36yquz72w4qwzgtzwmg790aznddswyar0pmmsklm8echpjq6y2e0t0xechsdmmyq6h5rzk6e6v5h3dzk3mxvekj5enwxdx30njzsf0dshlauznle36557zzvmy2lfjp2u3t5ms6mnmkqzrgat3xmzpae8djxsh2tzulh4xdm85452jyncc2ldhacdxz9rjvzek7a9fppcuh40azd23sd9u90wty3ezrxtgf2dc84rukepx838qy358ufvc97n8jf6nvet59l9ny5etqckgkfzu5s677847j26ld93g43926rpsppcvqqn5v5gzxwgxm3je3m06ltp92q09u48jd9u4rs5gt7a9c4x3mgfp3vq8k7275vhmake8tgdxpzxumyqhpkjraejtrpa726g8a5nxs82fqg3wf33s5spkqyrxn0d4cjzwy8t5d2w5726phtghl8snug6e87n3n8ky2du27hxhc8stgufxfwwq8v0wfy9ttzdjwen0zxu80fwps9mm3qctx92c7sq30nhw0tx6ft2pnwjuekhdjdf5egr940g77rvr3p4t5jsjjmdkvghp723fc8ns8taktmx35lgrjc8aujfqer6qhrp6evkg8rrt8qyrmmmuekwrw5hu5lcelzdd0cf0smw66p5keqc7j6ma7qvxq50t5extdtmauhzwtymlmeltu4sk7cjncaunxmhsu7nfpfh2aw29056yajzxt0xnq2kujndl8ndsd6ntn90mzkue0e9nvt2pv6nu5uy2xkwmklrn26cncvkxnkug7kq89p9cp6dk2naunwllw7funllluhnwavcx2jxqcxy26s3p4z50wsw2ctgln0cs05swmqdmctfwe2hvutsp5nme4jmys0kjvx5dxv5nkya7fxvlranm4e3x82j3ux32wz9z8va90a3fjhg8zt9a98pnq23fzr409udntvsyu0yuy58w9gr3hf5h8jve2g2wxflgy6xg4zx3v2yf8xyr0hl6w6k0cregug6u8f5vl096xf0npe3mqumpaymn0aqllrnk8je3eph8fywen8094yjuwxlf5mmsnjm7eggtrepyerpycqz5hyfhgdzkla4k2cdg3gygh492h2zysh396vrwyxw4lg97jkv44eyzl3nfgc7mffmjf3zgy6gnprs45nethknmgh59v42hpqnx2l530xhc4l6p7gmtd6rpra8vc45m60rst8ks59xjnys9s33pgnr8ttkjtrsjcr0uf024u4q6hz9tmm6wrhsq3zlv9xtfyz6t6kvd0chkvyj962psdjhwdzh75nyje5khg2g3l566s3rjnwkk0kecy2pzma3eulx4p3tf25clpqncxcn9fmnn8ghd03tultf0ftzmjd6m9e7nqqk4yguqt5nuxcc3hl4gjyeqajt7zjrvj5jecttcfvacfsxmz9kkgu29jauregaq6nmms76md2kzffjv9jc8lvkd7c9l2jqvps50mvzkl3z4v9tw8erp8n7q2dnpxvmc24ttn4yu704uty9tdm452g5zsdlt8jwuvywckacvprcytjauzfl8yzf7ss6c4uktrdahgfqkscl94j25q35vfnsps6up3zl4mzpglks9q4uw7dw0pw8h9h9mwa57c6c77l7x0yjas683z0sc9d8cdvf0jak0gzsa0wxugacrkylxrwqxet04hp7qym03ffkydp2l6uf9952zmtnpuwmmz5gn72ewpj4n3haveh7arq2pyf8drm757ne3zm9q46t2lhl2vyhdhg5qc9ftd04sh3tkv24qm9gylz685yp9t3hg30lg0aspfaq2fxme77tphc6hefjkngeq7fn9wzuuytsa40q076r7n20hu4m23c277xyz9jehnr53cx5fatvcl0x02dvg3l92qfnvcchv3yds4kq9n53cgtk032ck2mnc8xv34zamxfuxjzzk4g8t35c99g4ve4vf624d39mekv97l5rfc640fj8q9yvysy7xj4tard26zeh5dt859yx4ug5sg5w3pcfayf7ezhx808nj825gn569nglptll57uvapdkeqdpx02t7fx6v3yh0gm24pepxe9q33066t9h8ta3yfv8vnt3qk2yxjm", Network::Main).unwrap(), NativeCurrencyAmount::coins(125)),
            (ReceivingAddress::from_bech32m("nolgam1092cdwvgzvfsxwlse3g56uxe7ty9agyy9dnhf2ua2x2xuakde963w6aaeuqavwjzx2v6t52pyy6u86ppmrfr6wsxrqtmppvrqaw68vxtz2l7w4crzr5qf4gc6dka85jyq6z2y3uvhxlaj6lqedhn85trqgv9pvelm7wc0cquks3s8v37zdt6wcvsh6jsh4wkd9xkveaz3x3rc569vwethyw3p7src0lvt4265t5hpzsmwx07jgscgmvy2jk5njffpgw8kxuhaadht7tt28x8weepw7dpwqr2jsh30acc6jcr89zsf0v0c3lh7x2h8pmnm934z85nkzruu9k8wnztzhaam3k7wv0yzqh9hh76em386qhungjw4zcczgy98xlllrn5874cw527udfg8q59hkhmcuq3p6myt5ryj8vgwjrvsmmnpm5uydh2neltaacv3y960nxyth77gqf20krs3jfp30vmhhyz48dchr56w4lyjkqnrkxrxaytexumme8jh5gkq22s27ctgyxwem86qe0gxckk6mwwvllvdt4z8tzr4qpur33m74s5jsyerflmzx2l7qm9rwvm4ls5fa0tcfv6aegttzy6ly53tfh37lv6gtv3x4nr96dj2ua9upz3hh78fjsvqjtdmjxa69f3hvt290supk2epwjpgkzn6dzfqd94ndy654wmuy7h5h52kk7kwrr9szakzya8lffdxkedvdtcak0l5r7r74dmvc4p3m5ydqwcyrsk2qjc88pxyngsh9nhd0hhu53xwv4mxm3rktwla6z7yzddkgf0zsd4qeve4x8v9r8f4uddcxxrn6e9ef0hn7dxjwrvcr2p3carzfrg7g8kjhj3cra6apae79xv9dfw0x02rhh8nruq5lxd5667v8xcggm4r2dzvyq5fn7ajns0q99f56338jz2phlamcddxyq67lz2frpwamx6ft2tjq5s3j8ha02gxls4f8dfhjuw5422dpwt54j8gff7vkg489kl0e703muugent2gjf4tqxtcme6he7nn6q88x96ftjxma4js8juzz9hht9kanrrxzl06n7cszcxpgn3t8huxtzkm4qdenqq9g7cs4hyzy38z2ytnuzazcqqv36h3e7v5lt243e38t0af70teu95l0qqgpwkcp6pzh7kyauqu3d6897v6aarqugyppvulh49fd9rwp7yws5u8f9xqqap3hz9wg900vefxtacms64qy0rm2ff4s0c0hf4su6s2m7c930gmdxwv9ag230qkentl5hylqjjpxgxa33ey7uw3qjg067u0s68czk5ndj8ssftel6yyeg6g97szawzwgkkmd0xsuwthurzh4r2hckq8wm500qdh4h0sj8j423f3rskvrgkdfn3nsgqw874rg7pufyfuegpu7dy0wz6rmnhjxu3h35d8cyurdgwl0dej8juatqguvswrgqxnftp56n4csklz2wasetl45l23630r0vgkdn2ymyuvuaswlq6mxnsc48pggfaqnpe4ryhuzk2ce6ewuk6hcdt5h3mrtyqlkpyenwz2ukrtwy3m9rsjmkdd072ftunrxj3qek4ezjdxzp4ler044479acrhywe8l8ypt2sw70626pjmvq5chqxeq8r3jm7h5edp7efwmtplmkmw5tv7v0ztx6ksv95q3k6uk0nrw9kzpd3vgqf7e3c76zj8xlr5pe4guddhwnhuzsx2zj9yde2j0juhxtwz28f90qfnc3yd7gvutwmvhc0rww5e8n7u9vq60frlzy9uvc5n0rf8yuggy08kshvh36wuzeaazesaztyqvlfqq6aszlrhu4q0ad45tq0sny37z8jgg0gvxz3x6cl8ctztem3n3a42yuzsy8j55unx95zzgnsd6s8wg7ac0v6hdfhusw9cnga4zgkfv0ns6g06l3jueax7jxlvnq47x5mrlla00py6jhz5erecs2as4j7zlq0qe9fw5n7llvfe08s602xswhx5pkmjvpa0enqtgw4krt5a282s06u7tey8r2fyjw0jsaz5s0preyu3gknmwq87k4dvp9h3fe5u9zu006r6uxn4q4603rs5z4zrekgq0apxerk494ntdmstw0axdqwjqjtq7l6w0u3h7vrt070aqsdr5swce5e6s5zfwtus4u9t3wkvv3d223njla3c26hwdhza0jzyt622qrrczhx5k7uf37c36j68f00kmrk3genuayv3vs8xtyerg523pvl7mke4lcntdwte2t43ryhnez6fndcqgm0p95huvplju000m0f9pcjw2ng4545uh798m90ecyv7h4wkxj9e4zxedwgu5s5vzmm3n5tt0qfr7yxfl749hyqsul54lzqzuexp9lnky9ae0qqsveepqjt0pel63allngrlth982zrjx8k20k8t8kpkmdcxnf8xz4p5p9flqjskssmugxfy5yl8l4e0f24rrdghw2eapltr6p4t5mg2n2qtg0dupytckafu32pezqu934x5ucgyr5c8g0d9sxgur269cqc7jymfu66phkhet8w38gnt6hpx2lmq4dykf2m05ks0x4td4tr3gqs3dapl8zup9wnkdq7uqdnawy6pzm87wctdpmjnzzrdqkamzeqfuk6a2jfvml4q3vdgse0qm6u5s8gaar2az8kljk59hy7fqv2zmt8rg5q2wspglyq8f9q0vaqnxxk4rr5786f8pmynrxgf0v8th7s2x37zwc62ewrp9k59mxnate3492w5u7wqptmzhfzywt7tq8gw20jt77dv9s0ptmysevw4wja3yjp8z78y3alyyk89x7fvx525cxqf34mw2rmlsha8clr7a6knppq6cey9fzpzvkm68mxqhjcs9fnz3ptsswehed832nz0m0rannz9xjm38n6r03p6mrxckwyda2kydt3ynawlqlcazlhazzuptpjsccrmde43e5y7wvzxzddwu35w2kwkca6yca4xnv42gm3npjtyne2sjmy8ty2l8fg5wnde30fx4rayx2ups49dtehpdf34l6kny6fvgvgrgytyv4l6k0x9j4pg0979lkl9t67n7msmwcnsqm4pcwgt839nh2rul4lpyarxuugcszjetycks0f8k97lrjm57rkd5fd8lygam0fwt2kztvnxkznj93h7a3ydjapxasccx05l3yww00af8q2nxs69ucfnfq2d526s3tq50t4z9uyj9rlsl57ragcsk58eakljvqrrenqa3lpufz6p9ezau66lhzpcgx5etfll5wvz5wc0fsz89kddrfngujl09adu8xu06mp8pd8yfyzemjlzchap24nq9h24z7jz6z0z48ynh4ele28waxcdfepvs58yxqs4ph8qzq5wq69cm0kv0t6ncmge8r60gj4xldzge0a76wz76c3xren5ymjxe9vzj", Network::Main).unwrap(), NativeCurrencyAmount::coins(2500)),
            (ReceivingAddress::from_bech32m("nolgam1uehw7cnntdcfjj87yrwg0ddsl2etjs2xz4vttqelytz4hcc55lqtzwc553v3p28uxtucv35k5mzjzxspmx07ty9dcdv0vtark6ylw4ytpn0jza79c5797fv5e07dgf80r3cz88m4nrwm9lvkjsumyt3yvvv7qakutrmkq3pm097397gmqgzqrv0mcaxhqhppjmyttrxue6gn70anjufa3kg2vpl40h0x78wa20yp8st5pttw6en6l9j7xl4sa7c4srd55dszqhm0x836z54k56793v2wj8tym8avlzzxvh4rh4my56nquja8k7l4nyk6x4l3qzr3ar56svl9q6vt9ce2nxv9v95frcz5xhqwe0ugm0jdsmj8pwz9tp3ngj52j3k8qgpa7cxxycujh6pdju33p2nszak4yhgdhxqzhh56fkr4ew4zccu7nuggvnuvt60rkar57vvygvxlyy3xlt8am3fpuupa8jg4jel0zn4dq2a36w4na2vn79c8yzh4vsh8csvyhzpu3svr0jnr8t6ns95upas4kmp5acufs25xxc3mqkrs4dqwzaxr0yzj6a0jv8m7dltffx34axuymxvseeu22ndy6wfjt923xaaspywvfu08keylc6zp539q48ffh68vvyw09e0af8p0qvcxvh2hylenwhx7hj0xjtmnhq99lau8zv96mpjl5nym96fzdx4ka0msg3lp92d3tush8wpg63tfv0k0hyx70d57lp9wsuslzm9vn3s2mxj7syn8fsxefzqu65qn3k6c4q2y9gj7el7lqs3hhp62hlqlcwrcxkxlnsjtsa4wvhtrwm2qky8jhfzjmu4mgse89d2tgwwwcjy552sjt8cwtkkqx8hhwwu90nghyzq5qener6sv6gqtnslhn4tq3cpu8d2wrf7pj6vmyl3r2gzq9uztfnf4flxdr57dpshlstcxnvte993cjx9ryghk9z0c4ax6xx0kzymnannmhfhzqyfudv7ecmkq0pgrp8mkxawg3umhsx8r9a6x9w0209m0tj8r47ughcmpeg0hxfa0qemlngpfvjgcycglvlvplvzlrydu8mr0xelg7letkf7tvzvxp97yu75d0drm64la423xwu385e6unqzx6hzkywg62djyx904wu2pk377xzv7esz50tk6p6q0mawsca9n5hm00y5qj3upx7mmjf3ltlgcq0exhmcwmrqfwn75u96vvay3dkk8m5vdkx83htrhv8yqdms4gpp6p6edrqc0cy3e4ggde4auslqfh2qfmlfnsj8acjq9lpsgd8sx6usdmnve6mxsx9ctuuhqv620xalnuw78uppht74za96h0zeufv3ymfj9cys2zd773lckf6np4kjghug8v824m8vzn7qea5wrf8cumpty9tdqu0m5tezjh774xfnlkqydjc0qeccxzz8tdwp8kn5l8kmff8t40vn0ar49y64q0h7aqkmkyp2cv8g7sgy5d50s9773ujj02tach6vwk0fv24rzjdsmmynd6kl40zsjqlxak8sxq8w3zu2dlqegyakkw042d0f4wmnum55hzcawe86scdzt8q524s9l99m98e89xm7lmgzwgqpdw0xal8npdj5wzjfnz6gmtz7yugzhm8yf9d6mgqm702e4p47rjp2esk00f80pma304ls50rnevxuzstcl6pagpnryel6whzdr2erl6wrppkra3980qc83mcvyglsclrkx4dge07n5tkm65vhv2l0yq5racylzc4jd2tnllgmn846nnv5c2ptfcs365dug53zcephddr6wg57enylr6t3fez3x7k76xfawtzq6zj26hy9scq452lu65kj3wpfpc6aag0f2rzkfj84qkfl3n5vfwne2l6enk4a8j8c6j2lhxwlkvgwwtk2mzhvkumevwmpp3d2wrqkpclqns77xqzcwsu9hxwdw9wvfmwmpdwk0clwpf6lcpkrxaxw64f24fcjyspjacswssrtj5egslfyymwxrpwmrjjvnxwy0uz6hkswwn6vq5z6rrnedynkucj89m7tad05zxfth89zkcvwf9hqf9d8kkunde9xft5lw2rf60dzxlsrknq5p7n0vwmm5resv7m7tx2674wdj2ac3a35aswkn6jqjtgck36wps2a23rsz7f3adf5ljhh7jmmfdwavhkw9udxmrhymtx590kv2puwmxly0cmtnx82qmkfr4hlzzdrem5g2n8sss09qa0wxjwzflpamq62ca2z2kd46vcqjpg5g6xr9xzwvtegy7zm5lwsye3j62zftcc3fswjc2y94nkstqqzvwr4pqx9rzun8ez76x3hsxzqjpvanuuxgj7gwkcn4xf40fh98fstpm55fm86tt2m7v83df48uuxt487q2ahnesw5jxzr45hsxqgrg0gylw6lqv54ggpzldf23g60xcxk4zfud3r5qg8mef2txy4u352vmxadt4qx37xpr3qexpecwqm9pzu4fpdc7nalcartvs9fyujd7ppsjjpm0nga3xd67ngs7zxsmectrm7euwtar4rh23gpn35mh582nlck3y2dtcs3zg8de696qs69khqrk0un7f80z70n0xk0z0rra7uvezr2nx0rjrscwcws4ypgfgl4f27eelz8wkn9enf2hfj5vvd2hqwhqxycvdkz4slryye9a05jcmxlyf9dz50u2rm3tkkkup7jgfl5xljzsdrt9jhx42uetadcvwhj2zwvg25g3v7855yxgnazdvtzzvypdr4lyuvm43epedc325haktxrjja0clnty7u96clts68tp6g73nwu22y4zjdpkgd4wep8zc3vyraxq9mjs8ynnd7sc30zanzxs0y7ewexkjrmfyy3x0mhl3xmtcqzpyjlx6h802m9l3d9xjc8lgcqg8nq455yuha4ds78ckjr9a9x49w46q5h223xj7nprjswana0gtq4enyp265x75umwkgpn4st8euu9va8hwzuah0yzshc366373j7czrxrxky35an2mka8yljss2sag2ks2cpq93z9m8jsz48df4jmwu8fegwlsus8mh3mh8tyffuc5dtwjn6mtnkctm7vslz53qypyrx4as5ra6yu5rp6aq0fr56udgt3k22mqdrm3eg6zkt943h6u49ccxn3w7axqp87qwnh7fqng5zh4naggxuz43cq4mq2fy0ajusvsptu7gzejul3x9kcw8z5935s7uxdcjmlyvspysxsdnkagzeymjssj3vzhtcjgqpl93s6jwrym5ectxu3f2833z00904y3hjcf03wls5ll246c3muj7r5sdp7zl4fepr0wcyvf92hvkzey6648wqf4yp6gkwet8jsv6ew3dec7ctnr46qm0zzuhx64n5aumvrph8kwzsdf2hzdppcmykmm9jdg4fxytwlzud", Network::Main).unwrap(), NativeCurrencyAmount::coins(2500)),
            (ReceivingAddress::from_bech32m("nolgam178hdp4qqwtlczqfer7pjtq9g0hejzzqkrk8truevejrzmv5usneyk654uecl0073zmm50hmhllkvzwvn4glqydfw03kpw63jzjhchzfkjxv9fkts92cwm3ddemw35jlm3tv252c793xm2gms829h2vrnhx70rrc6rtkce7cgxvl7j64h8wz2u8s9g0gkz9lmgdt88zpx72gd5n57ja89g8kvynr9dmewxcmgyj3rvhx6nm80s4ctkltn2989xdtchrd4y8qpturfyxk6psp6cm2pgelzv5jyalq7znlrlctmyhw2jgm7gy8fdp9g6a2x83l3efyx0mk34mnf8pe9aefdfc7m0x9stwnzlus6u64cjfeyzjpww9sme66x4htvt5tmwm4q838qvx9qvcqak0c40uw87x0d7wzdaw9ys6duy333a6jc8cntll52y0dvqsydmgw47aju64k409u832pymvmu2r8ne5ujw9z8fxrd90j2ratas7sxd0zhzpg9ffydnadqp67vtvyhl428lyr0rlnw6sx9w904kg4v00tyfwdlk65wuff22v336uusrjm9ky65qu54t8d484ryxc2707zwd3z97wwuucfvknxn6yl9nmfhm4u8ah5acc9s5hrqfccyrwv0pqwmqckjy47afukutgy0jrkls93ec630pdy3t0snu8ps5lkapa3u8fwv8s3c4546yknayam2kcz309hkpv6vp8msu9d3hks7xxagvt7t9s4qnvl644ae3yuzedhafcqcatm3jy2xd3mu0dut5nlkt4xnw5sk8r6g3mhla62a9hsav3t4kvku24vrlg4f0zkyxrrjnlp2lvd9j3ymu650we4c64yq3309kgs07fz70lvykq3wugqwae8pvl0gykmselz2mvnlmym6kl3nl662zuktl696uywy9nv4y3pcufr9759jh8nj9uy0pgnunhw7t9jqkucd0zwep49r4rc0et6adqgj5u99gywd64ch3rn2mqhmnsrn6zj3jmd9ucvrhrn25nqlqwhqpn9s8knkf576n9sc5x6rc8haf96un23wmc3rg6t0cra2a7jem5ee70s7cn57sf9744mra7mmmnk7uvdeapt0f2n5s40jamj00m2tyfvk7xdpxlclv2rrem50cuk8zkdjyfsyeky9lkqq8c7l3w44exyqmydwwzr0dlceww9vfwy7c5ruckta30lsw2nkq0e2tfwhg5stkyhq8gsvxcw5hxtkzuuqmvkvml9gc7kvg8fctjsrqlnzj64jjwcrx30edsvyls83l5505gjy92smlvqx6z4jst2lqfl9qr0cm3neanes02xukcm22m6hhp8jr8j0wuznj0a4calw2tjygj9yy45r7pu3peu8vhu7g44kavcwldgz5a42zn8a3w5jdv9scdkjfymrfqnuysvsuj6wefw58um65j5pfyvfh6z7fjgerv57g783pcwghxxn2yh4e792nfg422dsf0qczeu8ar398ex6yj5hkvsuqhdjkd3ratxj7qe3l29slzarst8tquw75kmadekya02ge9pc3qw6r3fl7m2y2t07nz206wfr83qpm25upad9xa9q22hn7arnykmvhh38evn8q2kftsy37ld23242mj2m5ah5ytcjryww3lzjy2txae6y3tw9s6z7pccv9wpa3ey456dgavs455x8am5pt4nxay9fyfd4a9n9ml5zyqgnpzrvgycr4u5wsfr5mac289gmxqvcd8urxhmspx7prj9lkswz0z4xt8yn46qsmnhyq9fxy557vv82d4h4qzwwq0ygezcxhkqz4vrk6w8rflr5rdlwtxf49dpje6d2h5al8nhlwqxvxvape2sl4jmvtt5zn7e0hza8ewev9xtdf9v2tpytd0hq0nh974ks5rpz5qe2xfa25a2khmhc60q709w86usy6zzhkg8j93t6endzyfxhzrcsy8ckl56tqkd6n2dx8ef49kdxz8arenganr66rsfnmt0ceuu8glz2hqvywqkrz7gf7yd5anmfhewhsuuep2d8qu95gswwaphjk74d7ntaygdnfyrd6xx7pmwn7p0aavacc22yn3v7ds6syg7ywd9dq0ucf5n7ptflft4k0xs52frt8l6z8f8cjls7fr43twyjlneu62n990fez2df4hm3wcae6mmwnzewqnk74exafzncvn3drqc53ptm7njx685lns4mrz2u8r9hwex46rh223d6qwv4kwncckea36e0suku80erx2zy68584g4ecgpujpp8ajz4lxqc5e9hynedjknms33qhj5x3zqrcge8uyd3t92rklzs4khhpelnyu32k0d0dcuueysv8ml76ap5lc6slnqz9xz5ghe7jzrk3gq36p3ptwtufqltkay29eesremwu4y3sxlca5yhm82068485f3l24d8jr0nxx5tuln939dtlsjhxrqtgmg4q7mjkys5h7w782gkncqzyddhn4g3yzfa6mtmynw66967pyvgyms7dtealvgehe73sy3x9yluvt9qn39g3k3fm35hwlvxdhwzdwug73pazf8268sav8j8mc7g32h85xptyax0mr38y9zc4nuwr42sud9k75883d8rvln9w0nd65qjdml7zc6uexxr9j06u3xkkmv3xjseuse2622g4zt2gv349d576t34fawa9j2ur53s25l3n3lgqev0udf6hxwsj35klae8sahqe7qae2vdkrltdxrj2vccxqwgc8pe5dfetjxpnrn6j78q7ql9ryhnprhe0r46xfn52vgmzzltard0rf8exlj3ucudclq5yxp6ymqrefvhw3rndh6cvzmd6y0s6lzcppn7a55850x3deq8dvntgnvczl674rg90a3p2gk4gh0n5kkqd29avf00h4fzfukkhqu6a8n998sjtgg4datt6vm7y8c2muz39ey9v3mz7vnvmq8zz50f7mwysmah96taj5uya223hwegjgj7squzf2wqgql0rgpdkvvszphgxakf6ge4m9642wyug2thg0c0cluuhtt2ldyc2s0y962s9ja7yftsswjddmfrg6n3n78heqt5gh9jk6al882qh6e224sfsh6u0arn3h0vvvyz65vt073555tpzdcd6d5ntnxchxz2vc94pk3umntvhxl6twpxypv05qcx7qe55xkhmyfapgf08urxrq0gzggmkaljksuznxe86mes2xwugxweqmd4rgjxgvxp2z3hsr0wnu5y9jcy3shwdsjva9wdz6ts40435ypqp7s4jc7qdr6y0yhxtsgqnkly7wqfq03ghev2003jfexm4nyrehen50hgkvdjx7s292mregzzzzvfyxdzjjllgelrc8kfzu8z9v0qn9spk8yyufu6y25z54wvjczs8n698epcqw30j5qrua0ty9w", Network::Main).unwrap(), NativeCurrencyAmount::coins(2500)),
            (ReceivingAddress::from_bech32m("nolgam1xfstk6tu5qckche7f9j0jja2xstxpjmk4uym7ezh5f8lzdcc0gsh9588d0edc7e22hlteyt7ja6ggxfs2fauaajz4dme04cugqpdewk3r3s2hc0zuyupzyaa9cjckl7jufttql7m8ersaqksdv8pdgcvc7xdsxvvvev2u8uj3klzfdnzvf9834cz06jz8cpsqt70vez5g5c04f03uh7t5kqwsurgq6u4wspqa4slfr5jmy0uwkzqa2xe7kk5639t9a4yldwqmtqaavq7hyn3rcxdhkxk33sc7fz9jagqgspen4ez079zxdnrty5lfhzqw940t406ppqdpu2xera4qx8ye5an9e8qyhvmfe4zzwhq889c0hutm6a896k72jp0lpesf05qqmug3r33mnv4age7y56v63rnuzz822epddh4k7fjzwwga8gtgl7te4ya6ltr2j4ep9pgdajk23vuu8fdjhmtusjukxwrts3g7kplxk3acql7j3lt9vszsfmr9m79nxzxqsxxh6yuaxesr48snrrm0z0cmpzr7v9v4m3dhp3rhu8fcs0fw5uhk23mglwu2pue7k3ra5ccmuam4f6e7cth8f4gkxy3zg8rv5vujzmen38z9mgj5hym5ej4efw5f2cadkszeu4rj8ee59hjzgxfqxd8fwmynlpvk89ve8aknmg6m5qv2zrgrkz95pz94r7du7cau0jmnr7zhnmecmdmgxh9qzd4vw7gs4d36t8swt3873y80qgf03nh2uwhpp8sw59kuekwvyur0arjva3u55s34qj8wygnjm2tlceqhphy563uxdcdx5qc220kx3epntpajah8298qfypnkdva2mmk0xkmjpawenltrgnpeqz53ucrv5yuynp9r8qhgjvqhjzvgaw7e44d53ngzkeqwf9n4kuuqy2rh93zjczwylfdjyln3exxssf230kncjuh8wd82u4424rpud3hgj5ugg8nmqa6j5ythvugv3qxhnnpt6m0vtcx7sh0yu3vjasezpmagqkcv62558x23q83x92hd33ffpczyp23znl6tz02slg7w8ym5uc8hm6kq9czrl5hpa3dj8f6gd2qgykwahx8gmdg8xj2fxv9xmmhaz09aq00egxzfuyancxj8hwtckwskkrnlqehhjkxjjv434d5mt4emt86dh3nvw0mxfmtck4f9dm399xgvg7083sx8ldmgcyz6mwyaeh03htnyxsp2c0s5phnqgpxug4l3ugvf6jn2ucs609ujh8slgkwmcx6gmj7w0yz9h04tzfwhjhjxpu8jkgdtxxz3y9aqc0qvhq3epqs044k7wg4pxj2t8ggqxenzkl89sasrzas2vj6uujm22zq463dvy4ca0rr0hdhtq25q5alc02lrgnxntzlrkjtvpqtrqhq697ytpkcmxfkaaj3ym8ngacf08md97zmqsnsv3sjxlfwksnfjqdk49f5w7nf9ehtw54fq4nzr7l65kqg7nku9tdwyllqjf2x02v9gf6jejnc6z5ulxkdfjhk78cfdfwgvadrcyv399h70hv8r0tn04r2r6t5uapu8s0jkxw74h07j8futt07a3ch7rkx85vx683v4sl7u9dwk6ghx99pg59v75kwcdvcv4yvx4875lqu9w5akmm478l8wja9rs0wexa605st5sla29prl5r98pwf6dugs0e45hjs4geu4kp7g2cdvjec73s85v2xkfl8836kj24fyl9rhyjwzegp2vkm0tyexuady5m20quf2yalemhnlntc8lvxzrquydhr9fl27ce0yyk5gp4vaxn97jvaucg0s5ehens65kucplgkdvpr3zkhc0cym4zxdl639nhcfs3fhrrq85cs04wavwls7d6h3yt6qtnl93gfplmer5rv538uvtuvg8est7s8uwfqerkhrajllm8gpq2t3hshjh4y3nw7ae3k2y8h8rfp53a04vc7cpwf8pdu2vamluldpp7guffc0qlstk8nl3k9dvnx5d00k0v4ad2ha2z7dq84yl2ksezsa865qfh5fj7sc4tzs07a46q2dywqg65v4hw9gmptxhyhut3wndm39296q3klguzga9a6a2z8prul2srfp3tmdwc70lmp50nn0jlfhweqdzcjn8u2fz9huhkmfyehjk6rxm33af39759rg3m5gzlxpwug20sqx5z5hqqsymx34yevzyas4emgsaxef9feudff87s8gzfky5vqw9n8hulrqn4r2yl53ztknxnuetl7szxglr0syq48exqyyskpd3k79ph0j7v029lkxehmwew69rkrqjdjpp2dml2tztyja3vll848tpjp28zcnvkh458d7sgpfc97ktaswl3sdc4apat84hwxltq2mca7gels2zeqf23td6an306fgpgcrwtmmuj6ltky208gvmvxdfxgav7rehq4938qa45pz5vpkjjs5x6xynh873kt50jcm5drtrk44pyzw5a8mcsa6dll4xfq0ct68s4p4syfshx4e5ylexe2srwctssy3g2htcvfrz48hs08r2gwfn7lpdawh0qe7qlecske3yt045zapq3f9r9sjmd4vxu43267pstxpjjxd797uv5lzlfynaztew9wglq48d4ehmkwzk3697jwsev6fy6zr8kq6hudp9fmaqpysq5k4nvjk4k5n46w84ldclnve348vp9a8zqrlylw3jjccjgf04u2u7c5ulc93hc9cn5wu4xvzfsaekd8px0a532vmsk8a7ljw269s9v2yf07pvxe7gncvf2d440yt0a489v24adme8s733fu68a5gssmautqq99cxhjletr5x7sky8htt50cm5aenceh3wdevwrjvmzcyqf9gd3x5hv8a5g3xl47jynqn2t2ck03f9wmfsq9fkgwjsmycn9kc4ut577r4mpzv4yk4gmh0326adg95yza5svu6s9frmzd565kd8pa6nh656ftuukmdpa395s9hjnxgumrslf4tnmw6plkvq7e8uwh84lq64yhn5uhxly77merzp65pdnuzjkhe23wsfzmqw9vjhw5n93plaf7cy86yc4daejle9vlap8cymjzpphuulc5vzyq9hn0sz5vktyalreh7g5d9dw5k4s27qs7chk32wp5qn3a0uzk2s773dsnz53lsv04yvqs0yvem8wycy7qpy9vvqe9enr663z3qmgu0ykadcygacvqf5jqf6hy60d3p6gxjgrla0z3ddpnf9faaemaqed3ajf44nyq939k73qwydrfe2c4tvldx8x82qgceny3ah24c0ew0hv0taj9355rtsxxmc34jnrau8v6u7qpn772cmvp9e23ltxgy2fxwptz0aaey6dtu2nsdsldrnyvdrdfemaf4grmyh0254urlajl2zc2xr06uspkkhuszs5ukmm", Network::Main).unwrap(), NativeCurrencyAmount::coins(3000)),
            (ReceivingAddress::from_bech32m("nolgam1nfwwerfhwxl4g7n3wz6an4a8ph2sz4ehumk5zkgm50rudlc8xpzzf5gtzfhxxrntqhwd0dwzjr22kqgg27cc7fkw0tjkywa8tfavdw3xrzqslgum2tphjfghe2sdqda8f6wxzfscmsusv62hwafpd2xuey6l6j929ze8zlp5djalxj54la3cn3gfg2u4k9zt2e85rydlq78233t0qhn5nkx8qtxqfdaxt3prhsf9ec8uksrdtkazj96t7g6ga020alknukajwckpav4q3wey3trx6c80lsgqlhrcwxe0adq40eu8smdf52820s6afu56yfcr5xh72wrgy8wxzuzvwjme36u000m9fjg98mmc3646qxvkwzaw6xw6k5jvxz8dqvy59weshurvgmtnts6u7k6kmh35mrp557yw5yleq268vwrmp7eg57j2fmde22z2cxnjvlww7e7yc0p8uqzpgq6n7tl26zk8tuyj4hqku4a5cm63sjz0585xu5nmzwpuzexx6264r39vlsrtsjm8ayq9njmj5w829cnsn53g5gwsrd6n5kzm9at3en5e0da5npkfwzcsc582wzh7tww6gx3p5cq8tf2lxgjqc5rppsyemhy7kn94cx2t5s644td2t4jmlry8ptxm227ls3lpd78pnp70f5lxzv2thdqrjeh35v5m6nunp2ny8g2jnj8xck4vt6ms87qw6vzyra6zs6g9yln566w8jnae6gyxm4uad84k0n2h7thraqkpakf79jvvepdjtz4a8hq9943c09wt2e3jm9744um3dklsqszh6e0hajn4a5p3zjlj77dmde8ph2xwhc87cu52ag08vgrdece35e5xqn7yqsuk7wjhkpnap65zt93yw7xrwa0hqy3gvdmqdfwxk3xx9awtu79e3wpmclnkmdl9ns4tv7jgpn9s8mxqw8u9ctghw2nqqxhjyv2knfxqhfgurdt7xy35nzqszq9mner94etd0kk43fgkx7878grhz9gfm85l8wtav6mqf0xfp29t8qhcfnerz5z7jzm3q4wmtkhqmmnp85jmrxzdgkacaj0gh69f355rcrse8cuctslgwf5mwlxm7c547ngss8es4kx92y0aen6pe2kd6h8umqe9dpne4c7pu85d7afzv0vzk463xf70lnjszvc9ycwfxvzxjynnzjanflrwgwzhmxn4ata5pfvg0gqs2k3svsykywj8p0ypxrclegg9smzsk465svxf8qayl2hw9z26c8k6cxw339ecxlr48tvmh8f9r44djn5xrkj5x44tm07y4v4fjdvj57qp42r2wnm3q4eqxfhjyefk4rgqd8c4mx3m098epg4ggegyk7x0zp2pypdkknga4a98uhmwmggqt79kwx9vkz8qjn7afl2eh9z53yn5372ge07nnmsm7zavk4jeklwy58penxg9pdy982rnstplwt83qghhj09e36lplk27ugmy02x724ketta88k6h9ph74vr2v9jmedrvxe2xw0uwhkhszlfzwu53qz9khcctkvne90gds0vddj3e5eq06g7e4lpansdc5g983lyaqz3hkpmad84xyvay3ff36sc42v8rpwruysnuf0xy968dpyp9zpx6psq27petnclng8d4787g0f37k0h2vvmawfk2pzqfpnlwsp64kgn2a8uaddg2mzru5p0e4qexgpma2pmeumgljvvq80eym5hpwvw8vgr9ta8gh4qw7qrswe3gg54waaetmz7d5sez7pz2wuvhcsxw7t82wu8zwu9nqtqpcc3p3ux7l53hdynmjnltflgx6kwv7n8t4n7w5t4sqne7euzlpwd7rhf6wgvnjyajzql6a75p25j40s3uatwvuwsrl2getq30l60p3rd2zfn73rw0cwc2x6ts2hzk2jx0cc3l8z37hul2aws33thjedfp4dup3qhj7xzkgynvvcfu9q064tfmml4045u7pdapk4j6xj3rx2cpjdhkhnzagm33p5nwucgy5ucylyllvkhvzmezjygwxdwj3ydg3pqa6tzzphx23j3tjv65j4l3qdytd22eyqkc209gvyr0jeuqc2fx87yycglr9hullfyucq2930f7l7m97fsxjsqfywtpzfh9y2c28ghpta7y6dh8djdysd7jdy5nt2rlegud4st5enx7u0ku5srrvacfrjwjtq6gzys95n48m6j22d5puy0dgmklz0hy64d2sumquk2qt9rrf2rxe4kmqa5nale0afdt62ruanez9su0wr5nhjxt3r9wcukh7569t6qs0unmsd4frjc59qnw9p0ldvmz5d87mcsr36aggzjn9udkef30r26ymkj69f4e30rd4wclm6dnqjtuv2yzgckfdu7tqj2qm0r7xfg37faw4748gwfl8z523vq00r4prss07alha0whr8s6ht4g5wpc4lkzw5slwrd3vsl2w6mszvs4jjaa4h53vgeqrfe5xhy9f5hgxytq528rmjh77xnnswzpyl4kh4tmxzk4zyrz8wyszlmznl0faghmy3kqy02kaqgnykf06jwwl5mlgsnfchwtsgf7xhwkvy75vdtw36quqhsswgdwvgah5x5wcak2uncp26lsv5q0edcnxngqnefkaxdny22rd27wa3xxp9sncc3w5q4s4ay7c78t60999e9pgs3svdp60yuxp3mlkgvuhyt3z2gqvt3sp62pzhvrayrchp6pcvmu7lqwawpr4dm8vykr4rtftm5vut2rwdnjxw4r93eqznalajze4h82n3ae8quuhhd0xcz279pq6yhgnrqz02es7l6jfj9f223g6qlzs680qy7n26n2l6tmklzlqsvzjlkuxt09jlknr5egfd4dxng42adqhfey4reqvs2cfan46kn6q72w68cnkucjcvyjpxeh40djpg4n5qf5733keejqymykmcmackxq7lxp2mz2dpacuvm0s95sf0uvdhgh59rvqzzukucl8txaqhe8ctfq3djqazt9f8g2sfde8p60hwref88la4u6kj07j629zelf0xalhrt6nzlulv2lr82yyaqjag00mhjrqragycryjasyqrhezyanr9tpv6wvygk740jvs5v48794snezd6hd40pc98h97d7pd78ezzrm4mzplrg66r0wk3l0l2kw4hpfwaacljlgpnsydhq4cha9f06p6h9sztdy6dthkgfsc2st3m3q96xywhg8y6jde5h9lzndkrxags698gly9ksxjfzdh5vnfe83e7zxfr8s0pyf3zhrh3sku8aemhqzm6nyxt8dugvwfh86faes4mdhvsuqfp2amd2vn9ml3tfgkcwuh9ayvnuxu6kmef0jntptu7n4qjyrpjcsevyv3c6020vhufzjnv5nsknv8ze5jhv3cwascwyfrntp3xqjmg09xpm3cvu", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1agkcmqjnwm6ezc8uef6gahutt56292vmek6u7ldrm0qgha9vp6gsp8yl9xfkstyak0me4wxdw8rkrqyshlcaahdynz66ae2l584a653uwtj0a9kkrdejds068pe4t306785ufplusyztwyauw6gps0qzalv8x6cths0lqqv6u9pdp724xczeyr667ke4thnxt2x6hhflsr9r8jg4l5vptq08z8nppjq3z7mjyfx0lguac4ppl6v399fz5d0mj9y80qkrmru4vuajqrw7sjy5vn4g97sm7kx0j8uf5vqhupjn8vd7t7x0wpkg0pa306net2jutpj7xunayfsxfvkphspyy2czrjgl49c9s5j4ll9utyvt6kpzuuwz60yufctwj9yhj8pqql4plc7zczfpge7m0jp0v8nc6kk64xu5chr3qjh0k6tvtf4437zvvfkrvx3gn8dx97d9ypvxfw6vw3kucnpkuu60asj5jv78ns4zq898j5enuejwdchv50ckn40y5y3ddek7d9cn2hkxu92kadgf0sdm49f3vsfafg5v7lqjfeks7upqlwy3ykac9nu2l0ks9j9tnjzz89hcd5urz0tlkredpn9vvc2tw2g6hf2wmpzljwamy2c0p5haue48a4s4pxepg793y8thp8a2zqlu0tae6qpwl9l65d3n490rrgzph5v3mzugg7k0mjlq35dk03k885ygyyjcws8m0j3yhutpxr8s7urpfyngy0zp9mv9w7rzl72kn8j2c540wn39mvhmvzv3kcv636cky8d3lenrg2d4n60elj9xnysnr2s7e42gzcldtuay86fsgz4x4j68yklc86l5h775cgudjcln864eaud0pv3rfuzhy9du9vphrdl8sykr663st0el6lrpks32qdlhqjql0c9kjgfrf8kcsxjq9sxgm6yt6trwxxj6axtfeva07s0kgwc8y6xtxczwunfsusjh7qax8ycz05ghmmnwssa6wmqmg7wfm8gqlktuugpyerenm8er8y5hun4f57wzjslk99ccacu5d5tfephzm48gg9wlrhk4wmalt7k8zu34s399vdz3xngaprar8vzaf2uxt8tvq296vg8v7jegndesmqdwks0zsde8y592nx7efu4dqwuzylrung9eazpr8gft7pem84kw62cjt3z2myldml9w207s04x7v4glewxw0vpfnn5ytyf43lckj400k079krvfargnr2lhvhgp5wcadxwwq7y9k8wx3ta8qak0vcwjwjp6746ssr0dymayn0y8zn7g8naegjdrmjsafzg4rmle6y7hu9g2mf3vgvvst7cd49jym4npkdf2usghr6huxmnfhu969k80ky6pxf7upc3ar8ngp0yge3zrd87v0ktq2ght9s7zrayc94ufpn07fzkcs8qazuegjrf4ft3qyphhayz2lqemf7av6sx9kxwpyfjf0kpjlmxwcg88p9fr07fxm6cak478azeu0cqse9le30xhhpuhyj8tan067np3cswnvd4axvdpmrqdmcfu3aym6yj7xa936jp9ehyyge6ll49yxtd33mhy979t3j0l4yn7nrhrmv655xszvam2756hqr4ggmtyuq9mtyascqlu95u6rznk5l3azlslxqr9y0r9k3njg04cuktw89zm2pegtcyxzghrydvfljwd5nk4yl8tw7tezqzmfv5zxp8sk46ekwjv43sawyrch824t0c28cqefdz04qj9r6l86lpcht7t2cxte0jsq6v7guwuqw7xlcl5u2a0g8p6thcmxk6876f63nmsn5vuvajdc555nehzrv0m3m6tm2gdts2wl5j7sqce87d9hk27hdycmynfafdwpcg8hzvdkwntwa2fpde8lew38cv5ed2xqm5n5wazjyu86wq4gcfcy7xgvhxsgracpvmhxcurslpeq5mpf72w8dmgfevpz8r8wpz9dwy7a3dqvqgu3a0mcjd8vgvlsy59kzmw2h7xeq9uvlz2pdtgysmm5hhxujezgj305p2ltly3wa0z0cgmvn7t7rruz9tw5rr2hsmcnuxt579mp68eh8fdz0882puztrygch2nrcscsgl4d8sg52s7wsa863ptpymt9ulddznsxxjdpytfw0fl4uw2whwulswujjvcdz7dsmc3ppwf0dusf9xg96zzy7cveyazfkc6ep8qgc2c55lc9rw2xlhduyn2r9q6eyp52vwk4q9y09dvxf8q6whgzmty4q4jykj9fx0fj4uatdkmpuqq9cx76n83uyvej2jpdn2fruw2zuwl4xyrj8sle6h7dtczg0vya59hf95h0l88pn2r506hysywxyu3gvhmskhqqq8fz8m24gau8yusl7auc9wmvwl6plqy0cnhzyxqalzz5k54q32kjmxsyqee0d8n5xvdf6283w0q0f2zmgr7fxqztztqjyyu4a0qehdmpx7dfsc5xykfgqyrr7yncswxxykxm5q9aq4n2ue447mwxc668ylxxmlhcfrnfyept99v3lq2wyumzmkzqf6fuz6qgwk709t6xg84ng0l2sjtg20n0dzrhdzhhn9wrfhlt8pv2adrayrt499knt9qpepfa5vsqeud25j9ekazz8xpxurzwetjc84t5e9y2qu27pcs5qe9pw54dh2hkpn770py2v6cg0kygzlf7t0w75yqxcsxshru953k93ghd57vge0p0rru6k9w6g9r4jr0xayn82umvufmh70ugxup486ak4zf3tkyt2xmztkxx9sdhwm960g075kr93l3epy9hsxg7m2ues2twk6lh5gfk72zl2ncrerkg54qvfc88kgf3ppfx0qvfv09g7naufcyu236x2c9wvynuyfzmmfch9xkgs84e9tvvtjlc8yy3f8jze0xt45umqrp8tt05cpl86fdye22urk45nc8e22j58nhvua3f3lyknylk752vutej6hu8a72yzxzczqdqcnz6tfz8km3hmzdz2ek49ql9ugjwltnc8jc46j974m76qk37cnndkvw45deu86nr5e39gvh9swfyr3zcpg6rze7esruxsqqvyxtcmga6kseqtapswwr50smyh6z6lqj4fgc553l3yvz5kjuc2qlysx54avp36s4uclrt4puryzntje7ghyumeyyfjvswa8yu32k4s2u98cyumc0sdzpprw0kmn3v0wuas8q82wdm8g0r35k5e5vg70u74mx72rl6vt80p44gkdqmmg7lkqsc3x97lm40ht9tuu27j6g2lw043xgcwc9d4yxxfjq4dtjaukl0z6rx2s2yw34zcwlg3srh4ssqgwzmxtcqnnfltuj8af5v9jeul7sj02rv2hmxvqjse4ma2ycy7fphsj3hat822wee9y8fedsxjalykwk7swn8xu456htnf654r4vcjtsjkuc8yr", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1c2zjtanyu7cey5n8p8yme3g5yxct8y5j3c4w3p487yfu2zfc645tygg2mfqf63j02xha5jepaw2ppdhnt3l7djffysh3w8lk5djystdmrwa0cfn4rzk087uype2ey552qutaxv2x08x9rug3ml0fykyx4rnyjqpx4hprmz9cl87uscv9uhlzth8t8n946yfpfrj96yefgethvuy3gu8fw7mv833w4zl638l42yzusu503yy09aptw0za69658r278q6arx47dekrrwgr4seeyae2ejsuan50p5swtnxzv7uale9fjlmruryflqyv0xuq3luejk5238v7akze062svsgz09kj0cn3x7edguyvqylynz8fkysrfjlmz8qeeca9p50n5v2rm89smkh20se8ktkm39unxv46q3tw3l97syvlac0kg0rnt9snm6axvf3xcue8yqml2h6phf5npc5n0dzh5thcwsvgsgx2dxxk9cy5vyma0zlg0fdl0nf3ecr2v9pln9tu9gwtuedpn59ae6f7mwnezjv66fr6q7pcmppk5rvwqgtqarwlklxxztkltwqshsz6rep64s5dganr2magkg55rtrs3tpykax22scg4u9mz9lj5mzfc7rz0ckfc262j67llx3qvcn3sqqjsm9e9apggxla6cl55k6ltjp5e2qph5rjrvyv5v7e29yttzkhgjqr9akt480rlrsjnwzg266vvt3399u5tmgn807e6s52a6nlmcdj7sxg7l87ps2kkhg23mk8k2zy65edm6qaj0yk6dn3gmxccvza03azv79zy0uflzqtw0x8xypf3tf7jp2y26nm6mqfu0ww8ffllfyg8atqfp204rk7dvgmvmkjts2nm2zfhhld3ant4wxkay4n2ud7h23798dkrqgynsxdwejpk5fngh6rn05xhfspgkuwl8s46w9cj0zsh3ws5ewzrladw0sdysdv68z5vxajvxv7h37y7at7xl9thrz3vnmh4l0srtdmyr5a66nna37p23d4prr9sp4e07psugtfprrx22m3dzyysqedmn7wyqsvwex2f9cekvgqfv6xmjvyanrqysvamf2gtquatppus4aj9vhgrqc2p280mk7y4zl9qj7tcppkx02p6jhfdgn9jntgaxa2g434xpg02jy6xxn7u5wfsw0kj2sya7sxtw5rcpj74a7ashanag0rpvwu2cch4nh7lurqcch3ddaspdk0z8a6m6sh7xr2cm2sjz3t7urr8lz6anwtyv87uepczk4kk8yes6nj6nkqvgrns6ux5vwu40400zsvnk0xfc9knrsdkqfc549zehuz0jear8yvf707ua0234pxct7hnrqjsddw00edsm4d0eetuqlsh2lu0wep9aulaluhvzprx2pe8wcjf9cgc873hknfhlss8kdrsueq92pvqssqnvsw5lcjl4xgqqf9ys0jfzzj3esuvu9ad2xfk9pazgv4l6kuy5e95v7pusj5vh39hhxqtgyukulntac6h2gnnxdtrt3ehucxdqlk3zshllnnywruf0zqjgd0sudl2fv237vdr3psxa4g2l8fmjxwjvrv93dmyx6tyu7ddnvganzdn0duvunpvch22kpt7zavkvax0rs6uqlfcwcay5q2s0t9sdr9y9yn292s0lfyfgs4qurx2trtqpv2u7qrlaprlhdxynf2hvtq703nufj30vfhtl3ll8uxl06jgmv3tpzr4sj9ay4pxtp3stx0mvdlzdqvmsqfyrrx52q2cy23x226pfkncmn3asg0f4mrfjtjpak4t53urmezwehqsmm0mjq5sn3yrp2aq94666wfynwzjxsx6j0d8nya8jtt6ufwd3ysgksaamk6wjkx28zm3jjl8xmfhd2daddexmfeql25qctem29x0cyj2mj947074g4rgan0k6f0k46pn3erqvz0z88vasjnse5vhxuq9d4rdqcad54rfpkdm2adhvrjc9ahzmqf9pfhjhsjmx2wptd6cmglg54uzps43qwsxnpcndkt8pjyldjj7vmw2lzt8ejc9dnessj5d3ffyz7aqxa4ke9g2p4s0znrk304updykn89dd8supten7mr9ewcla8akjymc9084z6aj9mhrgrx5z5ekfxpk49e3pzmnd3rzrm33fm83quef98pa34tx9cansmh3qxuqzmwx6e47779puhu50ue6s9lajmed7hydwxtzpv8np94wemz8n9fpp45tgzfdu45m3qsupvq2flqccq4wskyxkuh9u7xv4pwwxjv44d29xpvjp48r9swtvnrkesjjzsp4yw8srk22f54ln7e05pst74h3gz6h0ux7ykh8xklysd3vfjuhvkdxk9rh8s5d0yy2rk9aj6qw93taspgff8gmk6q7zl9w2cmcs2xrdrh6zyxpx64fc5m87ezwfmxl0ywfw4skx6rta3qy6vemlcjg7ff0j8z6c6wr2n354tmv2w7qq7urc7l2kvvzdd4ls6f0ndrr5dpnx2eh5h02vq9t6a5jcvt6qppfwas6lkmcdaw0jxld4exag9du067c2a6p29zz4wm5gqug8ltdjcnwkn4qrenjw6fx8r0gl2tnp8gylem7hv5r39yw7tmhcez0nav0ydf9ep9upwhs99sz7qqfnlh9eczyzc4pjtmhvl6g5xf7q7c5qrq5mfx9excx5fmd40upcx47yqjud9mz0mcpwy0lhje9ku8dw73v5lgefx2sqtph3a3h7usmfemhc3nwm2l7thftxr5apjzlypsnsjdk87022veq6f6mdrug0mjjvs3xwqr0tal095kw2gdnvxlnfm6569sl33dhugzj6wy3ayfdzr2je24dkkjeerk5uh3m9juun75e7962g53q6mk2aphjvfl8kmmlwv8n8z7cm2ggrg2d4lalqcslcmx48xtf7unwmmpxw8vsffl6xsreeejlxzh2ffh35k4pp9n65k9fswj88sc9twfs8qg7cu5wfjne6qktnptv9mj9wjk6ufd2trtvclw5w57ctfe3fw30luc3p3cmsdfpvmresk29ge4jsprk4ecwp249prgmr9daftsdu0ke55vk86pdhvz26utm8k5nzv2e6m46h8g7a656x7ghs28cj6lpwkrdp05tvx64tngnncxuxvjfn8xdfdcrlsaq0w33jw4zw7nzqpa74fhh9gwnl48nsgjg03zts99c3f7yssjrt8j3ztdpwd7pflvxd924udrknf8t5htepwvdvm26mk0hqdu7q942cr50kmmqaxwyarx72zaa9pj0r0w5vmq2l957v7h7wdwlerz9lash4pfy6fuq9mz2xn5dat6chq69a820qmpw8s40rtlkasdf2ez3gzgcm8jzqwpn8v0ngw5qus3c2xamnfaa6jjzta7paq5u3pq4p0cktu58gv", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1lnhe8s94hc6h6frw8l7wefaatg5xa7rqjmr38ha9j27lg5nmc852l7ujk8uez90dsx0wzya920hrq6mm3jnw7c8knh2p59p4rfvm3y64akaec8tcu4fa3rmm6xyv2wmvv0khfexspgqf3xaaf4kf5laakgp3jvw63e959fkchz082trxwh6632j52zptrlxw99s5resvczdey9a29kdeuc0zf486dq0rhnk6asqrn7f45ldtkl6a0a49z2y38klt9sdw22zm0ptjrcxx5hju8l8ahem3kwhspsn7lcr8svnpr4u0467fagqdydhfymgunpfpph8775h2794g5sxj7grppv3v8qes3jgvn92e86p20mz0cuzd4can4js9t00elfg0rgxf7p9jr7655j8yn77z8xpa3zskk3m4rww4k35hpwdnyey3m5vpt6a6z0z5d4vprrpcdztw8hs0725jxtk99mnu0dvqc5ygkjdhtf32mlxk6cnjexcuq4s326zkqrg02m5edqjj0qp8prjsjlrjy47qnkrv7tkz8e93pnua42tgscy3z6cztn8trdhe9pw6yf3why5up8jsy7nkc6qyma8pegqel0kq5z2d5u5hs2f0rs8slsffxjg8xpk27lg5wn9fk6y2qky0khxwnpd9uf2alphz3n6m3vghagg4c3q4ez0yr65vkkwjsefpj8m0nqhpv3ml7kc34w47nq5u30v43028z5ffmcr3kpggfj4vgv70yx7rjx9ehyqntqg5zyflj59f2n9g4mmn86y0cvulujr46kgwesrd398ud38jl5ammp55f8zc7wsk2crqnv5numf64juhrml67ahuj2drm9alhn5gd2gv5fu9r77zdn4spx2q9llsa8m8r7pyn05eawqnaget2kpnn9az9h33crkvd53jjt2gztlhxe5lx6xmhffzzuhgdvwyq8hrl954ap6aat97dqkug7cvkvavffh43zvzu8jv9l3w3txufpm2keutycsk3np9atgad0jrfaeuperjk9nkhzzr5rz0nf3r6dy3r2lam0yh46j0f5r7xff8kx94x4hkdxx7mwk908al0kjc8xj966cu7vjt7g64tj6nxcem728xd3frqsa4vav52qz444wkmz73rxn03tv8c8yhetl6884727lkv6ze8nu65mlk0m99895jgw7tx7kzqhcmsf0jldr9v83j5ek79yhhfz5lkdqkery7d0p9y9s7ttvt6xcq6sngg7djyp9elax3hq4yyj0s0uxhsmtz5s5ntztpwlx2uunn65zy7wzn9yp0ytkhe74kkftah0grvx928zmke6w3qj8pv0nvnlp0445sg6g8xfamx4jegpxnw2hv8n8yraprecaex2pnesgx38wklpzy8mk33yyngnyukgnd3ugxcqta0jrps2htxahuycs5n0vqwtyyehh4cu0kmscqyrxqswj5q8wprn0rv0dy9sjj2y6x3rgdk0da7cl700r9f7f8wmm26y94j6khrqf5ghcvh87e85tv7q2r97u5eqvs6mhr7unpkv5y06hx7c9wpe642dgkze8rs7fhdauu0qyxf5x5e3jkjdsw7wgex32vmc0vtgdle6atnyh4uguvwqfkk3hk8l0y2q4a0z3jyh0s6yqdzlwdr8zzv77ycnv2pm227j39watgyetwgj6trhtgwq3j783mqw3cxra5yyyx3j3zlxzupszze94s3dn4827m6zvxy7umhs86wpqxkhttjx2z2cq344smx5aym0j6dy0uy8qev4sp3xjneu95w7vynjhvjjl4fmhqgdwhdetd00ck44v8xmvulqex0k94328za49j4l3jacxw8tj0ws23ymm3mvtvv8p7h3d2qlsglqprlrqfyzqvaf657ss3gxcv420jlkjpxcglmduzs5hceumg67t9sled4emqr8qkhrzwzqx7w5zulfmp8w5nt9a3dgzy4rawuafug4k0q4h3xka8gwv6563yw0ngxu3u58urmtfupdcvepj5l2hlkavlvt9lkkd73yza9stp7wtwex8r3hpyrveg7hy8xsqn0kfe48s0kfy3mqg6rjd7ht5wmh2yjvtkpxpjl6c9dsukpu4snca3umflaqhcmv6jfxg7u9h85jd2362pa5fzy6p62m6ldt7jgu4cmrhl2kwuqgel7ufmqy2xtfamgnk0zyvzggrzl8gx2aft4k9rsufmc9l4kd4at2325hsap96s2u5046vh9vwu9jqhm9f2lgykx62uuq7g5fp7nxdr0v6yafr5wp98kmhd9vr82f62qjhr4m4tfv9nz9l9sk5gkep5rqwskch8yn2lk9gj59zgvd950fpac45r65fhmaejkjvrjafevmddrtaxmaelg8u6mmv37yy4w0n7q2s9ggkcx6tmafduccqfcqffd3gy2ep6qapwpuzp04hs8japha4mtac59ylsq8d2apaa85jts932jxccx3fmt258nf560z80rkq28ztscc2e0f6n7vvd20wkt0gpyndn0sjvprpqgunz4qu0uec54pnu950shgnul7xzfrej269erf5zng0ffkgs3xhtvfpqetw49gcynd0afjflmh8dx7ups9h6r6ycrte7n77z8wc8esr8jxcyxvcv540a5ejgsgk2s5jc2xh5q47rh48782dhns7kplla5ewf0akyu0jm5qmpn2cehlyqz7j253x4chr5u292he0a7zmkhvhcvmn76wce2ym26uhdy8qa8kdar978aq5d5f9e7sxyu6utrhl6xc75n0rglnae962dy3q2j6fp2rk6akc3awjcsmq4hptskf99ghw9lqw047sg9gm6kvphdzs7pew67fju5qh0jpdx7xjexa0eq738n09ay8ppw4q86z780n67x34g0j3wy4h459fpxpc8z89sq0pzrjdm8j3p3t9jfnd7emektf6etp8kx8wysrgtsl30533zda5juxvpwqklvzsw77z5259fmvxqjvcd6cyj9mxd0taxgtvsg2f3cy4xejw3s7yz9jvv5nws5uareuf0rzah6njqacysmh2jnmagpjg3enwnp5vg98zkaxcrjjudtfugptvf007quu7y76xckamsw34hl8xkmlqskt7yyhg0qn48n7cyyvmpfryzpreveevjaft8w33l7tgq342hmvxm3zr57svf3axejhkm9jvag6leq9vwz8l6j8ug2qa04ngadw6nn6vk2fvkh7gylxtkhnscp339y3zsfkqtnpyluq9d75q9fmtkm3lj5xhsegv8832sd39the7gulvuvzj29us7jqsvdlyc0zz2x0mlr2s2cdc9uq6vcnnpw7fhwdnlle53nhasaft64yek2njkhndl7qw0kqkhepaqah5qxvknem6am5as5ew6exj6wtmpjuyzx4d7upvt87v", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1a02xd8safm2q9fa6l29scwhuu7nds92k5qdt5ls0yg543rf2khy39mdazsg6qcylzmucpx0nmerdehdxywzf5re6ymxx9526fpxy5qkh0mukj6tltfj0sdl65w3unh4uw4j4pq70aknasprrv03nnqzxhkvedgts35xr4ym3zpv4f7taxjj0q5v94uq8ufd8qjdrjt50n8pjeaenxwksgyvh5gtmdxa68x8c83wc8gglldf9tznna4sngxk5u49ryq0s58d60lerjkn8u2j9q7w8h3ghzz0wdvglkl7p96gnyrek8um0npmywefz6vtux842eskcqlskvzu3nh523dqlprc4pn8fk6ns7nhpyn4lmzr5c3s3x0ynsg2cfum4kgxzwtdhmnnq3j23pe5flglnsfkuzskl3pqmtswdtk506ksemwlxg2a6f72pzrtyhe8y3lwxgwak87yvxk3550nj3evakkpe87yufc48p3k0358v4j3l0m7e86chdk9q5tcpgvnsj89466uvgl5yqmce3prw9sf9n5dkhqnq7n7pzffamaumqh6qdcfd8pcda7mffng4t3gmw03l3gp85jzcx8uwxypguramu6m5n0m55kdc6ymq40eqn7r25hgpfha38sxzgln9rht8xrzvpm5t7qvv9jd6yqdedt0wz5m5zt5k2s74exw9hzw9ns7g3pj4tw45c9jj8g3emsuag4uh35ca6nuas90u7nxdqew0gvzwxn0lw9hs0f6lvuvjp7y9w2vztune6auwzy55h058dygm5j7au2ystqam8llgry4zwm82jaq7jpl4fmzxwd6pdxff2ul75aulzy3tz8zjanrgdqgfan59penrfmheeqsxxxu9nt8s7hvkzzecxgy739tw809997ma268f9x28jlawmlt858ku3y3znxa2utfwezdvwme0y2xj7gu70qx9786qn5xgd4j3xdgtz67wqj79h0elv65jkeedwcvumn5qlt0v469pn26npm84mzj0gxapffhzuxwjlm7f75ctztnncmgvc632qgqug2n68nwmkyw05yt3m556urtlj7cvy9hat0m48t9fx8cg85w92tnwc7nr04fxrs35ul5y2v4kawuhrseh038rtuuhty8m9q3j6qhmmx24s5tllswwa42qm3ege9shw7axq9yq0lcg7c6mrj6dp7fq20utrvdgr84czkjcghgh360t2ytsp5h86mynhlpvph7akqxdnxq28w5hrjx5zteerwtadaqhansnj597q5kah9jc6as96x454samy606v706q0e0x4ay7r4zmkumefvz9pllsshy3xpgp3pq54tzg43qxuz5hz3jtsdnvtk6fppsh0tj5gmn7fe2wqjeu9lrm5s4qayaruu5ztrv82yk4ztnw5ktqdcgh7q5mh2v33c3t8d5yd5gs02d06vawhwuakus7kk6dqju7hlj3kpg56u0yp7rkxauj568se6vgms9zpwcjevf9ctq7fwq8p3f68k86gr7kuzcj9aet54j0mcxkqjpyjplal8dqwz5yv62pjhunh54lfnhjmrwvputy6gvt4sqecelsdhcrfja8tar5e74j8th8jus0zgum3hezdky2ftfz5vzxsf3gsfkhfj9vlj7hqwl6sgrw550cv6sx7cddh83hs2gfdyefaur64547w2htekn2kv0n2csdrz6va84rkx5l85zkf7zflv3e2n6a8glu0xgvmffk6jgj3d3v6jd0zjzj80xhhk8tsaftywj2qh5zhrh9vxxtnpqgu4zp4epdl3gcp95wn9sfzcmyxlryvanhft2maz2j5kzqqxta62fhusr7z82rpkvwehacfqm4lqhrl3k6gdl32v5j655zeflyxq9lxe8zn2qd3zgx4tn7p53dw6ual3d5kehr8thylqj5tgj9lmfmz6mqwhxujcr8q0nnse3sesw2cegwrtxkqcqgkzlvtxpj9s54nxt9n53qdj2cdsz9a97mvlh6xmy6q6404jrer9pa78fjhnyclat3mzjy8ec2dajh4mrvw7k4m6nwh0ycmsr6vx0y8eqwtj5nmz2eqrz9wknqkq4sl6mfv6j7g0n66ncd5c8cf9qmu5qt57gefwpax204yxr9zrl9g3q3xnrnqd597sxlwcxe3hyz3ptnwmc4v9k75t3xrey7kqylucyzglw4unreztn55dkq74re7gvk5vls5tfj6dg36q4jhk5dzucdnn9pa64m8k5ggpke09aqny2nljjvlrt3xjvp2mnc7crm2c7rzzp7m8h772ptsy24erm6kmgdj2xenzyr2vgpze60um2ue97h0wz6nmazk3ts3uv73hru2p827863643l620hwzf6pshyd7dav0j28dcakkqtprv3kwd4pz9rkyhh784gv67657gs4w5jwvyrte2qk8p8zcfh7jffaatlddc5yja6e5rmchzg2hzgqj5p3rel62s22c7uqpvd4lzmnejuwj2gh8x4rcjn0c6y0cxeehp7xlqp7lfm88f6tu8rc4zmkl5lu0f0gcs6pgqd3dkmzhwarsh5vat986mxfdm5gvgkmjekd3rfchsxstmp9n956qjqg8vg22ft08yaem3dphgscxt6zaq950t92tjc8q4q0s4hsu27wf76ks6906exet3vfezjec2ncn53y22srflwuuyhdk9hvyyvg7lg2taw9gaqt648c9trtayu2gtl9th92907ml6jhdkj8239scc7qv6pewht66mx3trmcx79q28jn38wyndz0m0pnphpt4lhnd2gejyken4gh580smaq7u8jecd99c4eakjk3d4ar9xk8cwj9jw6xycg6tkvyuer8fdruq57svn25qx9q27c8l0m57qejf7rg93g0ccd7f52gusz8ut4ktsc8x7k5jwsntxekw35zkjp3rx77sc2wettnmrtc9h606l93epj7e5y9h8tthl09y7uj7gpg0tkj4kgmwm62n5pxp2afvvgz9wuuj6zdd0zkytaj8f8szqe46c9akhqausrau96f3g5pt3gg87fpu09pqsgg3c0fhprxudwfjehdev659d08aareemzxxfjx3ffs4z7sqmznwp5lck5uhfrqaremg30x2w9qx7zwx28vn6ud3rfk7v3hyw79tl9lhkjrechwqc0u4w5yd645ezk2kqswnkgq8jhp7gwgmgsa4t7kqj5nznrjyzrkhhu5fqyphqamhg7rn0vhlp6u4gmqz0xd09jzzp93f8zy89h2aka04x8qvnh4xy3da8e5zztfx2l0phmd0227q2c4krhe7s0v22mpt4kryhsgtvr2j49fak3gqpkqf8g48vkmfjcpycld58gq4ztd92qzk468jvvq2ge3np3khh9tngvyxph23c5ll7xvq2l2fxkjkd58lkj6455pq", Network::Main).unwrap(), NativeCurrencyAmount::coins(2500)),
            (ReceivingAddress::from_bech32m("nolgam166kycqn64d9xnxhdekxk3g7aad9zxct024gye94y5lum79ww9gtkv8a8t2xwgmddzd0e5km9yr8d88puv7sy97fx5d28y56du430uahec33hj07zd6hyvtrfupt9yum7j0xplq9zcvknxpm9c5w45ptrj68chss34ev6rewvkcx2wdj6yyma5g625dd4ge8lvyehdf6n5tnjqjyuskk3d264a0gye640yv6m2aha0ug33tg6xyz4th8ythg7z765td3xl9uwt8lvpt6fsvw02rynyp24cm0vpjhqg8z3vtrpjxy5qxn29xaf9pccqf6wx40nnemvweurtlk36qz63drzjxyjzcf77egu5s2qg45njlztr3wzulrpjqwun6u3eaz8nlw58kuh8lepyg2kfyav5jp7vt92yrzvefj74jsxj0t6hz429unwtc4p8gasphfr64wtfxwzdcyexlch0v3xmaytnw87ym3puccv04h87gre3duleql80dudv6n3v4zgtvhyayjujxewvqrk4ufvdthzwsy7zr3cna8hlzkffptn28g3dzs2swrh56fpjkx6sn3rg8yj3vaq36h2gsfjh2t5kearra3zgf860tmn4gavtsry2w86y6kcps3z472gpn42tahp649au3nccfva3xmv7kc377myujz2z0g3sqkxe7gnql8ldlglntdkhrpmfgtu4rt3xq5zeezy3d25ld30vlwr47k92mztksg7rezv4sdszxm0ap9h37y7xk2kd88f0zfnsfncmxht24hlyyl5keq90uq36cyg3jec8k0mc7fqy57gsg96yslzps4rmjvf6z53hsf0e0u0pjkuy35e3qs6nqj6jh8e492alnjkyj5x8zuwrx50auet9xv3kfpt2k37qldz07t5j04a0llcrpx3n43q8ckdfqwz6w340lwa8utvee6pg92y97scpj78scelhdjgtlt5amx8vg3qjf0eaycj9g7tdx7nd7f6fvkn4u22ka07rxca6dfzl39f2czpuw0ds3g9en98wmluas67a33pvsxxcjn2tqhj7femy045763a7pmxp5n206th5k3p96dd64ucy8w5lsekmkqtplgyt7w8ta7yap3v42e5yhegn5nacvh55kgpehp0tz8sz5k63lyg86aqdmgl00s622fqzyu8nns83h64ekyvdydgntnyl59ws84fk3p9agfsm86wscldju8nr6fhrdmuz9zadgre2nmjquf4venn7tuzutp5jcksyu3zet3n2gkqmqwch8wmn0a09mvc7mwetjfqh0z7uus2ml92wdwfducenuf0zzhhj563zrjqn07npg0yu6rh66s52ujw5y70pv46aeqhtznutxa29sv8sww6y7pf07lzj64sde7xgka38k6807hr9ryfyjs3hyd8lm7cy29shee8hffcj7w26627puzwljgqc3nzlenyr63z6w4jyjc9d5udk8a8ve3vxvuv2gttvxqt869vajezsnhljl7y5r8xwu8aups5e24mxf50fm6h4zl4ledvw7ywa9tfyzvw0t44ezj47zqh6nrh0zg864d7v4quaw9rjxfke0rkd2zrhewyrgx7wvtegzgjdt9jgj0ffz8jsxn9ltjxzhgwweynyqlf6p9rpqcs9xff4ywpamqnn85wzkjfhujfqu2q6cjdmzxjaafva2jly8j45fcnufhxw40tnf8axh6f6x82gcwdlv79fdyvj5drjz7cvtt37zn9zfhk03v484dksdlde2nz68xsrvuqn3hehct74tnerlg2udeyv6arcdpuwafuf9pwdkl5fag7u8w22f7dswgyxe783qnhynhzcgamfaxaycnafja9ld8km6clyyrhtnlr5sfl0f6fdmh5p344kkkpdur57cc603408nwyaxxmx43f9934r0gsql9k8sv6hx4kwk7nlfwfpujzxp7vugppx8ksyc7awuepdd0q6sj58ku8cvkf0a6k2l4fvdrdvecurp43g03j9f22yc6zuk6xl7tg77sm5fdr9y455vhj29v3x4u8a7ttma3m2rmka0wt650cz7f8duvgelnyh76s9qcuf6kxss6tzyp0tqwrw89hrax3r2t69nqycqrjv6rjawevux3jejz8jjaqjequc9du0m5pn5e6s9nqt7dgclr39t92me04594g9usmn7y6fzrmwuy5dlnq3u02t08fvgjlf8mperqfwq4rggjp3g5wjrxstaqq0n034zpyl6s6hrz2ffrjtdxq8vc77q8v4engltpujr9sd7yluxnlaaw7jr9wu2hzdwvucsz5e0j50vgfg7q770enn3cmzv8ltqglfztcpgnfftr53kqecadlynmsz4j495pp9qet6kqear7apyq3xpdnj55jcy34lschhv9kn6xuypkypsennjtw4cshpgd3q3gz8g03wkslhg3dlhrd7zne90tkkpra66urrwys5v6hl6v6tmedgq3fpypcwl6u2cgslzmwsdls7weh45lcgy0knu8zp9ejncyv645gzl4eh3t9qqtz64rq62570yr49crf5p58twm2p3lncnevsev78c6wzh3dy0s4vrxadgsdtf5xkakzv8p2uxude2qseshngk9r5jmtmwdna7mm2w6arz8mg2ru3p455mzcy29ch3xjtc3wnqpupqkngp8mdl4lr8z6n67u9lhz9hzgpjyxr6k4fm8smlw29w9wn5p2fs8vplen9k4t6lp35crz5mqkhm45vrqnqkeqdfqtu697yvf023f46ugmfr5uy57q794cexdsdscsymkykyknet2rk376u6hzzkxz8jtnry2rnwexrqtmwm36rlee8q2hpg5kv4ep09ahc3zm5npchcuwcvc6df9cvwa0wq55slm8xqjzw238l8rg78kjfthqt66e2vpefmjw7hxkxc6q5hytwj2sredfeum6gla4jt5gq2lpnpepjyy42f7ma3qnr9rnzcz43ud7la08m9hrkul9lqj8nnahkxyw96atm2l59tdx3k9fgv7tam257ek8kykf60j9mdnl6p2fpzgrx5dl5g7x8j8wqcdntc6rv9z5ka5qnqthdq8awz0c2x0qhpn9xutvkf859wfj6hn87dcqynztvn6e478052ghclyj288s63ftrdnl5tj8zehqtjxp7k2cdrm95enjzjqehj83lquu8g69pakxr8t93x53mwmgz6fn7a7c5x7wfnsey5t8stfsaqlvhtf0uump507tf5j5f3mz6wx5n3kavhz3s9u2cyyhxagct80nc0wsrly86h83f4ahcdrh73phvueksy7hqa7ap3fpgpxu2vt666al9cud5qsvyaqs68hstk5nw4se6np8vt2z0prw9kkx3fr37s69phamck9gxne3nvxp2z6zcvl6ly992pz8c02", Network::Main).unwrap(), NativeCurrencyAmount::coins(1250)),
            (ReceivingAddress::from_bech32m("nolgam1st3yerpnd8nu7hrnrgr2ntn2v4dn0j05s25y7qaa5xsjzg5acm9uh49tg633ltl2ydtw43zz2rnj6ta3u3qr9g09z354l29eh8a9xha70f9l859ggnvrmjua6umkwjytudwvj3y3rlkz0yqkd5x7tdssdakmm5dq60scu4ev7dwuvw3hrnmlv3prllsf99jktgxj75fwa3nzr466lm3ph2mhs3n5fwtxstu68uttk9pyht6dv4fruxhmmgl0fh2wzgc9a5sxptalv3ryxemcxnqvl7fh00plzm4lkrr3r40npmyrcclzcsxqrc9qerusjs2zwf77swn9s8ksa8v33z8h6d29n2t5zlewf2trxmyzds454tsf4km5rpckv5l8htyc5ggw63vzn09tzpf0wjqwkyn5n3t3ky8zy7xt8e8qcutjzgmpwsq5ys04vn6x63fhq3tld56gsszgzypq6jfafwwy7wx9s6f2h9j9h2wj7usv2z56gud6vt66gx6ppwj3l0rguhfn4vzk5dkajsy5vqagvl0a7pcv4vz8x2tpsjpsv8mjdf8680lyme5yrrqtt3h4ezrxae3ludl2t00f5zfxr9u50wltfnmnqejtljpsuh3435w9236qxckw33c94xwsylcazx5a7mpmwz0v922v6rqv7pe424ssjs04z0sztzsqlda8t2ngmj3ac8ex6ypl3j3tzegrm86dg0pq0ame75lkj9ct3z9xpygqauvn3uzm9jzkc9uy7m4zxdwntp84ujehuhumj3x3lxju87k0skrwn6l2d04vyenaugswaufx64wgcppj97hpq50jtzmk452c2gl8auukhsnnywlp3yeyv4yu5d0a54r254ukyz7veth354n75t93f0vhpj00jwghwxnhukw6uwmj87cdqx8wyd07lu7v3lyk5528ueww5qckyc0p5kgkpy2pxnqlugy8t0mqladxhsvwf98v3z2ul9d2x0m8dvuf9hya52h2rf0wjy55mkklf5lgc2gwtjfhak0yc3hy4ufkca4r3456f87l4uzda4lwym9a9qfg3r9yc0hw4vq5qf04c8dtdz0a8gr3j3gu0gxpvafldl20t46w8snfqhrzf37n4gjdk0vvjak7lp9gq35esyqfdsadzk9lpmtd6p9l29snkpyueextywwmun676znkgv4guc456zvu9lqhu03ruwytp3vzfxqgjl8yj0wpvnd8pm8axactwxqs97a24769k8nj7zs8u3t9q9d78s774uf8ksl37vmk47d4lllnmh0hsujfdledlu97v2msgdvetf73ykrskyl93suvuvspxz05gs768a7e5uw768x5uypg7h3rt8ehvkf4uv4an660q3090dmjh36krze3d0fj4yt2rs9hz7zcpd6d58w0eu4hquq90kt5tkjga8p5f7mhy6vjurdjad63ze46wkrasmyjp3nrakkc7r7887gnc9vyy6wgw8pnkg9a3plze3hkq7ua8fnq5swsqu3ydvtkya7nwwl3xspkyjqv2cud7pzt3nz6v2g9afxzzyr3mpl30w3yvqyvjv8nlsm40d85cyxjy0pyzqqtuh8vde8w7karvhyywd4ff93z92spckx58tlpqpgzf0xgzhpr3xeg3pm0j0qz6hspzphhj4tvv9z5twudemlv9edd79zdkqa84ud9x2jaz8emyuhj0rlj7y9rgvan7rd0nxwfp73ulmlu8uxnl4utsd8rwu0vn6cxjntks3lv2505vkmu8cfss6j5vt0kw3gmr0cceexktwcrrdx74a20qr4tqx6087kzsfuclftth87s38u3xgs8hxns03ddf6asrd2xwrsjag9ql6hssup7qhf8hytampyys3y8v66vn53v0h8fn660w2uqz4aweqn7yuetk7jq0c4e6p7nuyp2y4x6e864mulqh3vzw3cxqq2l6qj9thg74yxt9atacvftaaj6rvzdwyktvdypp9xfdz3aszcpxyjpkmhmxn5ysyfzyv4uva3mr2lctf4c65x6d0xlvll7rvj0knyz9yh4j2c6vktyke367tuln3sl827curfddgt9ujv0t0telr2qnj03p9nryntytwuxjs7qwxvwu76cwucwsxzx8ttr0vha2sn7zenwaw3lqfwmw0mfv45dfsnk8wz47vuqdnx4phjlzz5thxccrtpannuqf620stafaaqm4ldjsn3qfpmj8cl42kfmka37dlux77xzlf9dhxws9e2xwydcj4r3hp8tnhc2sy5tnpdld7uhuhjhn2yxam6fnxqr84pe9geksz8nn49ldvazqtg09suwwn3z05u7xf6fheeqz78lzx52s208ppsgfz7tlylmufywh8shc63ewrqkt8nmmn82dygxx5jpxqz509ca5042veyjc48emkkltf8c0f32h0jpnveu75l58r9rkqetaccnyc8evhaumwh0r2uk2596vvmawe43w8sfs2w9akj38etfzsnpv7t0yng0zzgqnzwgqjst27d2zp5xn8sfh4mlgdwlclzt9y9espj0epcrzdy6l70mys326u52g88cl87hz5nfl54l2wf8z0f9ahfqf00nwykg770xykt0qn9la3hna2rwc9ws57hyu05t63g3uvwwy7agq6j77f9q9vcydxf20adqt2gr8ggvaa6dyu0kvap7gmfh80dj53h5wy9eg6xajmey3sskjxt8d6nsd4q7kg4a3eks4j4qc5wh57uwm8044x2ngfkw6703vegw6phrpxryztzr5zduvhfshqln8gq0s2q7qrr83v6qw9sxf3kpfr48wfs0lqe7tkwhhxxel4elce236a6jsnhgfm4k3yvftscl62hzgvap2ay9kuy57xj7m47cc2cquruc5fmll4d6h2dshzgeay83splzfpvtsj4kvj0vxdw2tjwyu7jhksww27md5ckrfllke53rqexxlrz43zhledfn0n2d6u84wuxlnjyd5yytrvcq8esqwyquhjelrdn8k7u5fzq2jls533rhdnfw0cud060hvlm9znd6ks9tfmjdvfg7r6u8chz2039xp66274eqgs26wd0yx60axnmfxp3cc3t7cdlkruv4druma5q06vxac36cd2gtg308qsecmzh9hx24y4ymcyxsd8xgqzcywgls3keyrx78zwquz6vecq9s0f6apc7zgy6my94c7gdudxmcq4ct0wgmm63seg9lqn0j0he0srmxthk6sp9u9yu3lg5cap63sm5dhagxzj0s3z6kqs49zmfjs6atxdkaaw9nuh0dwmh06gg8ltxny6hhaflq0ay69j4yxstaza6hktgk7dm440wf6y0p37dz7py3ndljstv9468cl5gtfel564fnependvw7ehsayt2yfzul087spd9hxkh2jdy4y6rhn6y", Network::Main).unwrap(), NativeCurrencyAmount::coins(300)),
            (ReceivingAddress::from_bech32m("nolgam17etuyl5d7ruqruf6fj5m7mfka7jjq8cttz4avs7s59kvck9f2ftr6s5emg5t56culafp7qmuvjnvqkp0k25qzgu4amq6edul5fwwevce6pktprxz4tw9eapku28fn4qfwuzn9z5nq2lcmmsny6qe047hcz8enrxl4tnwgkzmwrfkfxgyw3uaf95fkaz4e8rwthwzjejkpuw3gddwhqst3yrhjzerc7ngthvtcsqnkl9x975pzujl5cccee74e5rlgagvmye5gt54nhysqx4kkw7appl9ljp0gg237jhxunmlmmxpauujlz0vtx4vctn0lzkamzaj49gy48mh542gz3vvn5242lyv0qlm2sujj438vv4w2rrq6nvk59yjpxs5hkkrywcp2f8rluc6zyfr203jg6qe56luqw4z6cnz767nlggpp5mty24u7st82ll4vzvygppqfm63smwen0eujzc7thvdc6r54k9cn6rww0umstp0plr2nemr9y5f85smqffqw257q5vud7eszawslzaspp7jdl6d753r3dk0xv7wv4kfe2t9yg5knwnhn0hq80xmepng46umf88ncvmk5xwytelv822naf3qv39eu7e28u5l0hnxuh9jgduuyx68xkrs0f3r82sxa33thlprnjm2hxwawkgh6nuhlt29s24a6pcd46vz9x7stum35agk95wy24d5ctntt2gym2a03hhcddyux8npey4f9cddrhpdnp77qxjh00jt3w7adrdlqyyawwugjppx77r2ll4qxgcsgaydz570ze8qpl5st2cf9kzspthuy8wh3wqr8lysa8d3627wn35daljh2lnpwaf7xxlywgmzmgh9j3jhkdpnvcc937g8y6pa5jagefn7vjkcvkxws6wsk6d4v6zmajkqysfgctq6atv475cpn4ukez3f2mnp2cmtzwn4smma8u077t5xdlxnzq8sve3vdahes06mx3jghve5mwkjs60c9f0e29eqjks6wp2awych4usnjtkned02fp25wddnksdys9xll5jd8xlxjpmqs2wk28aun925vmlzy09eajtqlpyjn6rktsjwe4w8a4drhlvgykhpgjj5lpy6ce2xch2nurarc36a06v6qf8rqrv4339sn2zuggu9lsfpssmp0ftmtdwsx86ufkmgdg7tzk8aj9qkfhhfz7wj656q3wfxegv0s8u8f7te4l0eu59q498s6y4hy4ymfmuae00ry520ck20ry9kzx6m6ta4xnmtzer4dzz8dpaluwqcs4fcm5v5s2nlnswgzkcpptsedupc6wlcls7atnn2hlpvchu6zs0pg4sfmwxp49ckz8mlf9suh7sjcdx233mz5x4hx3uljrastjmtl3jjcdx5uwvshnzjelpjrvdjhkmmd2q0czp27njsnfytp77dvu7p7cjr9rf30jkyj6eeqhka43hdt3wfsrqyd3r7flwy2xa063zucxwnrvy2fqt40eszp0lywwdwxxwpqctfahetlp8u8cqdkwlcxzpv6m3vgjh5lmx2xjx99day0cywd5ttqwn4cnpndlltx3x0ng9mhrzt3a8cplan924sp8yyesgz2ur90pmd6zju34jw0ht6wjzvd7wjtz7sy9uwvqmvn2tpvpxukz4l87y3lwmtxrg2prszjj3cgfwy86k7ua9mektwntzueg0jtqwvs2ju973traz6k6tvdqjyhf5y00wh6lfmr6sxul3gq5dy0ws04qpqfj2f48es9va9ph7538qu4ckfeuastc9w5pmdlkwf6538uwxygdrsrwvyd87mm663tvu65d4rdaq3yx8kynz6lnwj9n4dfj45fy35ygmuuqeffsa3a7v6ttgs2c0eznj7gks9kdreym503sp9cu4zh4jtdsurvrwml9yf25utgedn6hr04t0fmkwdcpnuz20jjmsvjsdnh3rngw0vqjvd9j9c9r2udrz0vuhn89twcua62yv8t57zejkh94n9hclvnhl7j8zj9rpmrntmfrknm89stpjwaf9wujp70nlr3nm428xarcanq6rmy8f69w5vuklxr57wl2htmdzhz67lpaknz9meuntkykyylz73eha74p82pke40cxy7uskd5dpf44wj7yfj5hppyfcwxe9lc0qlf9500suy4ka0xn2ldapxya5ksz3a0fe5xz6rx7n0h5h2fffv92ch2cq77mdzhpzlrck5dh23fs34g84ttyvhmnka6h7wz8h7p7a944whcdvkcrpxdcjpmek35ykmxph9t5vrprsrcsgjmdw9qmllyuswek7c4uefudvt63prhf3gmrvnj07lm499g66mlxzpsyl6l8r7zjnxd056xfyzsfaje7f4jv33fa9gsxmf8m34d4zw32rhqzmk42h4txe9a9grvpjt4v0ngvzucl8dews5gcxenvulmg5pteu00jdry3svgfu8heyxh9twnuensdundpw54v4eazl35zlg3lrafxj4fe3676amarc24njnsrs6l28htp0wws3d8ueyraxpncrxcx6gcy2scpq2g6sjv7mtj88qkazvhsm484mp2q0ykek5zp6rpygujqsmrdkv9yndlgdledwnkgfmgd2d94ez9t85jzcpcwck9muu267mpd09yfgkpkg775cetjjet2rqqg6v7e8ycznlms7tepuejy0vmkdnu8w3v8zxxvmr43sac97ht0y7mjgvupru638mkm4gtuncj6fvma6pgtng5frhxtuvxhg2hs3k5nyzjmzf0cvq4v73zy7p5azuqf2jf7sua4n3xvaqt58fr9qhkkdfql4z0t7u2cgfsm0t5njp5mdr7apjw4e67qvr6s5f3vgdw80pcv0tajm8xkks9dl0zpd5n4n85zu35c2jvjwqg3pd8hdf7ny6l42cgw2vuh3qwj2x4nwv03sne9z7c8glxtgupfpusl78sm06xtj8hm2seq9pv63ckdst0vmhq2ekpdz7frhmy9d5wqwp9uhu8fzrv9hu2zx4cjt8l28yzfdkec4hqfy2pnhj2wfmzx0a2zwpclyfpwt2l53u0x2nf6wygevfqs4tr5nntedeqwu6zrd3xcrsuzmcdzjrrlzl86ueyjfzdq54d2hl9dhe4qjmwehjfyhj94utqsnd7vfr8jj0k3yuexckq7nc4uywxyz7a454ersqlxc8cgyuw80l842wzldrz9sxjgzgmmetxj2r337gaey9vr6klqljzfnz9fextj3ky6qadvq4xhkeuv7tep9484ajangq52ntw45yyq05stx6zgn3rs88sss7svjwl48pvrax4adh9j8tel9j8yvcel8u2evw7zxru5fvhq8rwn7umqx4wq8c7nsfuq36hwmhr8y8ce44lsc6zd7s5cn9spz0lec2lhte9ud2rpvjp26pvct66fyd46", Network::Main).unwrap(), NativeCurrencyAmount::coins(20000)),
            (ReceivingAddress::from_bech32m("nolgam1kj648lwt8rvlajyfn7gvxgjpex50sfymgasxagx27nwadl2z20mkqufgpv6p6jp6xuannp5wn08zwa59ldhpswhhykxg54wct0n7me29m9pdv7a23kype0kvt50eavze2qmrhvv8wgns7nuc5rl28rag0uzgv7pgjgwf6c3ku27r7sfudjttc9958x5dhpw9u8a6scdfmeqkp3q7pttnaxs7yu30hyvvf3kxuj29fjvr662ea42th7ezc7umma04yxknt7daz7ssl06swv45jhckcuw4txg3um6j0e3y0h6f6w5gwqm8stx25cqcuvdh2a2skrywdafau3wlhv2usmlc05cgsec2l9te2lfuuw447uh7p5alyprkuqklc5rsy5wshg0w858f29lmkyd4a2x59r5hcjzra0jnnx4328d72atld456drxnm368x6cnz5atnmd92m9vhxzqkzrwvs0twwvvgae73zuzc37kcgwncxz2yhkp7f0um7lh9jrql4fv0es8teypk3k608kjtzzhr9r7kgwhm3ktvsv49jdycfwgmg3099ldc7wh2z7vuc05a6ww7a0e28tns5wh5f67j35kr05mtzr5fnsweaecxq80hzd76jas80yx68qdq9apz4sl2cthc6fw9aq4lvhuc7f8tvt52kvs4l56q79gremh73cpnsghl0673dqdku8ygpwkjxst4jws0dsrn26qvwz3acy4kvggxzf7syv4pmqlh5zum9cwcpxd6c2l7uqc2w2x3lch3tashpgxhewg4lh3p92g6eq0yvwmdcsqv490vrut35mfddfglrtu838p46wlkg228mlmgs68uk9590xxjxgy8l2ed2a9yrs0aygpf77wec873n679s29j47tecqk5v0vsfzczl9x4yqwxv3djjtfe3qc5hvc9u50kt8uyeklwde35x45pc5jydw7hmv3eufraywq2rx6luvd67fsrmh289y6fad7hzlncsl6vjwp4zveezj3qw6286l5ftr52s83p6ypeznesy3w3g0fdvhajyrf5zl352adecryg9gzh7tjft3qxr7q2qe359xktazhce2wut82c587wcwvzu5wrgp8zq6nzydkaz50lujnt0fmh4jdjqdq90krdgusyq6n0tuhenktdn5h2k98ye4y3k0830xa24e3d5y7ypcgrf3pdum49ww5dw5028zhrsuk9s6zg78l67v9ea7jfygdeagy4c9fs4ptgf3mk0a2z8n93c56y3waxkmut3h2ymhnlelfjdh3egnkysxh3xr9x54ddlv9xq0cm5vequq8xzsfcydfpx7hpn9zfdjdqrwjwsjd7sqczgs3enstlvgeylqml7k4lktay5j3rdgkdwxmvycxm9s3x569cxhvv6dce5mfmet9lnm9tkd4cqzwyat9qdweqlqw4hdgrlmptv27syh5pjdssalnnenszc0amcwqh35pnq7ytpttdhhypgfecy6cqzau3kzxnppccd4e20yj2ztrufvtalq4q7dsalrn3jne6tnscdn5jx48fymgmz4qq4zzgz9xysrra4laxcltcvjhv2uqwdqajuqhcc3ka28mkj67c6kg6qctzpz25jncmvc89kxx8x37jsnt3j27m6ak0du7zvn2efu4fcqmjpqv25350wdpy5f6cypt2d4tea5tznvwl4fsq0tpa4exytwsgwaef6d4p8at2rr5l0slhzdlxvz57rp4kgrt34lwkdnhm7q4y7pkqg0dxtg8mk7jm90q5r79fley7524khw3uwadsxe6ckgn8lqx0k2zkksgq2m328ehsfzufw7ykx90gd3c3yy4ctsr6zau2jgd506p6hh3494h9da2h20wy33xes8qyev3ryffk8w2vjmfav5ph82gskrksxcf9yev8mxanall5zfkgduzq592vhs9mekfcfqf5xmx0e5k6yy8nuqwnlk9frfqqds42ucxgays5pymdzdl60k7n545u9gjq9emgc565kr4x50ycwfk56wm7xhqc0alkcqk3fja69hkqqmt9zthhrvjkpagmhaptyuhy5g0ptcv4r6qkzen2ed8aa74ddnts3vpmarrxm46qnc85e8w23hzxs9vnmx0fhtcgz7qsua4zg4juy6m6ca64ann4zjqz0h7t9zdg9kmt52aekkrmjnfgg2ea6humcqcsukhzfhceqez9u73guh6vatwelhvtavarcuzzvhw7p0ax7jel5wusr2w4dwj7ylc2y85vxha9s6z3du3agausvhnf8snzkmhm242k8y650g24ggzgw8kpwd548vdcwzj5q4p8anl7qwdaatmwz27k7g6983jfc4x3e380e3n3aujthxnqktse52s55vdu7ts5ljp5dqar7wzvq94zul6kzfnq758keq7tqmheh2hk6n7j2fxzq0h3vjuht2sxs76jx6xev29gnqx6geltgxphslxk5wwajgvjes5amkd2hzavzepn4t2hxnh4d575aej847k8yejp377g0g52uw9n3s5j0y68q72dap2ug77acdjgw6ha5rn9apkzg2ck3xmahggqxj3svrfajxq0n56rkjaxgh2u0wphy24j536007l37z4xr8pglehcurj6vvjtjgqxs9qsmsuv0slss4vj57ww05xlyy92lltc0x2tqvl4yv4emyhaudyq2l7rwrlw298gyp2r77jjf3s3jdrkt6csasu6d4k6mvsxrxlz7elcrdu0dqecc375d9nnqtryqj2qwfzzg3ww45kvczvp9rljgre3euah7gkwdn2c6jj35z5wrl5p3lrj0pgnl6ykm6dl9p2kvu9lanlpkvzfve3wtjchetu9sdl80nx4gqw6e02dtr6y93p45jhffel5cgs6umqv8vav3hjkz3ru0hdkg057ptfvkk6z2lcg4ejngmphq3t7hmp55qlkfe2dqecpda4hw3a2606fv9yzdkgxjjfvk5m5jvalmksp297s8vje03h7g8wsyyaq7l2m55nyt2dlcsv7nkn4keqel47lq4u6d3xvvxun9hx7c4etgyppcta4lmdpp3uualgkqtjxphzenxjull8hrh5kgx74v6xf774qqnafr5y7xlzfy4fv2s7xurxd67u3cl7xd0lvn4dcrlu23vjjc2457940v6et8u3tqheudlnk0qtr7gev33595mej2ew9we9xlayh76l024emflwxr76pe00d22tjep9wgzr7zzega42l8edgazhzpt8pxjmlneng390xvyqpm6gx9vxhlmm0ljxm6kfwrw62dswuea6plp2jfxd4nwty6wstpkml0hf0pq8x2984nd9rh4mgl0y6edzx7qmqlh6qq2laew78cmp9dxwa5pyfdcuqltek8vsva6fkf3tg7cc45ud9vrrrtsmrv06jzwrc3g", Network::Main).unwrap(), NativeCurrencyAmount::coins(2301)),
            (ReceivingAddress::from_bech32m("nolgam14qddenqd06cccjeqe4x23zamf7mrxmv3syxuydvmnzghwv28l3smd3lhdslxrzjgyfg98lreal2lttdzsreqcgauymmt0xev08slg7yma4d4wprk5g3anv2gjgdslalhrd2j6xq8zts5zdha3c5q855zg36kuhx83za325an37lu35ludtcqvl4ycsuat205nlyd5tahaufj8fczah0kc9qkz8fu76nj0lrxm8pfdaj59wmg6x97muw3jy2hry64v42qcvnzd05dgv8zx5zg5846dgcrn3jy06u7my9jkj9d8vr0gxllpvgcqw0jadlhmk4hdn7wn5kee6n3vgvpvjtqfqdn39z8w27knwd9cc4hu27jmj773zj9rwchkk9k2jz4sx7f0js2y47pvs64eg3lmj34s9lapyv5jc4dj2y6qt6sdkgl0slytvwfc9udfgzl73tspdyqyp29k5dm3cvsg9v5rhymdup9z5852w7qr2vss4dvzhug77lurepcp75620d0qyqecd3cak4xtaw4y6037407p6dkakgn2hzczmr95ctdwqga5xay5r67yewvjgx7snyus2fwt748gpq893wwngcz05e7s3nkaehtpe970340hae5acels0nejssc5ghc4xzcg2w50m5x4t3v33vuxh4mfckfhgpclrccgssdvmat7j4pxa0edt2dcvn7l24q5ydfxjg9eecrr47qwerxrmg7xy2lhar45rtg6getf9xheftaf4qhlgvlgqkmuycw67wc8ymdq6vkngcvjuzwu808cjhqcx9v5x0tqzqv6sq8e0tz2mnlkpk9y8lne9x9a9p34h63jqad0uuav49rcwrtrjhhjll7vawpr3vv567vp822gw9fd4j9zgtuag8rzlchupmch5fej84expfwsd98gt82nclpe0rxsm7r2ue52lgwe80tlaav7dkf56vdya7puwpy4vy9kdfeh5x07lvnwpupppcfkndkq5fcl4ey36fsamqy0ysawy5ff2e05x6svsmzkzaq5wqv6eplc2p0gqqvmnax9t79rrwgjwke9uvdg4y6rcfjz20vdt8wus2ue250fq36h7y9v8m55nrxevyu38rrcukdz43qhuqvp6rpdhf2ga7gqkvhn4ak70fvge3ccx7ddhasclyckr6q39vc8k3uxrk5hm53uh3ps6k08q2t94qj2623p8ng08mhqt40vjfwh0l846505wrrkfk3zmz5kftkd7c8mgtrnm7zda0zwkql0cgezk6gg0zec2fkndvthf5fc3agwfw28c9tmxfyu670pkmj3gjuxely6wad8arm23hpusk7p5yq2anpud97sv0q7pctjrmwq92nduwuvjmwm8d2c7prw2juhx85uax4w72d99l0u5knnk4djzkzvkz5y29w4dxaecanrvqj2r3n9wqvvg8xqtq96rpr84ckkw97g90tyxuarljwtl7lfxj8vw8vauh5t5c0hsln48ygdr3w4ny9udvs85k05atkzm5tev9fv7wz92ywwncqp5248mkqcc2qkh68uj05x2m6vzg66v84pelgjkwrnrcl0rscaqyap2efalp53fhnfxh8zq4rwjqzyp3mlwczdklst4w8fcgy2k444htnkxjxucmh06yqjxzye4euk853qjpu65pdw758v5ea44ja8cwnf5gjwve5hcfmyw3xy572nzxpuamgr8latfgk9u5mzdn3l7ams347vq46j9uyvkajmks5wmffm5lzf78r2fulyq9jymlqkzt0c6uvv9jq42zj5w5s90k5ax5695ksel56apvktxxjgwvqdfg0xnxvhjy2cvhjx3ddrejdtpnhk4txqcz43s5ndvd0d6hmndeczq7u92u2d89vla8frtcwul87dq9c732m85ky29e9tenh0twpfm2td22gc3m8tkcrsshp433uyzchpsa9nr9e54l7jhmez24y80z8sxuhksqqh4vj5hc63ac5pczel6sljz8fe3xgefmzz77ezvgfg3yhl6r3m0tz86d69c0cdchn7fp5xflpgr2xnp2d2eq96za3769dpyr687scute8x2rns3h6frtvt96325zypa3cadvg4nlgx73ep62wpqet934gmhc46axehfnfyq2a9adgysrdhue9qj0gq5q0pyfvh6k94fxea2qq6kle244x7wpde2d7r9kvy2fggq8hadusv27x50gxgh7w7ynqsxz8r43d802c2ewm74hds4scwh5784xzu9fhmqejpgjglvt0gwj4yz9jfxmljpjpym83zu9v0z5ptexzmr9cjtyrrthwehgql4n5kdhtj2ar7aekas62rdxyyvmyzwsdvlr280ll0pnl7hwaq622jcmzzc9qapu0z9z9dkqqe02puae98x546ftcsglh99q526afmkqvmqtx9zcl2x37r4qte4dpvdr0kexxrustvclvcqant2paptudepefcfdsmnzppdskqmxxvvdaua4zf724jlt8sjv858r4357uhv3wd08svmx6lny0faz9gegn9szfet94xm76hfvxhtmv2j0meqsnen4vj9zwpqtda9rc46z7dsx9mz6kvxzyy4r53klfrgyshl7hx7cl8c8aulvsymztm3ht3qkw7fpm85wqdgs7z98l0vys759lep9p3jexdup42nfqv4mm99q3afp20rwvwcky2564kh3yhrrme4d5ln0c70us0u6k8u7237suvnza6z27qvyejn9gj37rrka7arehd5wpuupd8dap79l4ketnq72mzn42s5rsus862f24xks74z3c7qchvr6eg5weuvvqj3aq5jkhpqtl8h53qj35f4zz4tdrc2w33n5v6qszjz55q6eyamnaxckck7n73uuwy9344ntgnvs485nrxa7d5cfdcfcmkr65wd74h6y5tn5rkak44fttrw5ss7j383cnv860dd7nht4aaydnrkp773vghpsk6ld3atstr4dlecp4ertztasczmm38mkn7xn4fnvdhn3ljgm4jgmxpzwt9c58zq9md3u87tksg3rn6k8lkcm3gf7gx6upelqc7lnfp8atppft2dw8whpr9cge28g975ruddtqrk25wmp283cgpdd447w2ll74k0058cy0m2dyrl60racakq3qfdzh60vhr5gqjamqjc87ah0vnqqv3unatktq5w2j8qwpwfsurkxsjh6d64x8ulvz0zhtqej8vxl53rwkyjusp5dw6utvff6vy60vngv580ctq9h8upjv26xxtppf3m2jsmn9psh3cpue2a82hzj5mjvgydzwp3mzesqrl06x0g7w799rjym8kxz8lsy3yxsg7qa7haynlgty7tfaup9g2yc24ngqe70d6d460qqweyndtgk2sgnahez6r6uyeel6uuqwsph6cykgqwz", Network::Main).unwrap(), NativeCurrencyAmount::coins(4000)),
            (ReceivingAddress::from_bech32m("nolgam1vv0r9chpd45jw3dm84qrruu9r27l90tq7qczd7jsp0mgru98wtj5hc8ghx6y5zmz65a2dg5e6jvqecdd4vql4hjjtper8pkvuwlzsxkvhllewujk5g7k8z6dye90acsvsckt8w20mxj6tcq3hnh8fxlegj26jshlty7c27gc8350053xg2sgq2mxhymge6330handpqyv0gfsl0m5e24t9p2emdn0vq2y0p7kts5h637v60kur2skdsc68lw5w0app9mmq4t25ec3c3h03k89sdt8m6gw76gs3cgdc3khc2dkzqkqd78z3maep40k7rhfcuc0l2mmd5pznjd3zfsfchtnunetu6ht5sxl49sna6k9n4nwpzms3y95zkf9ewwt7uaw8rtxh6sv6t840lngupfyae7r99ee5pncanufmxcy7wd74vsukmra6m3wtr03x8lkt54jf7pg4sxxahfgy24e8egn89ahw2hrzprv609d9rp8w8jgshh9r6pm3t7pwtdnxucgy87w4lmdnutdmur6q6p5kqenmvsyjt44s9hw3akkwjkqnedmdd2hhfsrwv2p2yc8h6jrfkh4quw7a36elj6thu0mlmed3jv3us9qgg3ej4haye6ka4asam5xkl967wh08py0lezdamy8ddfctv8638g2c6lkjhh2skkxnr3g230cuhs9zj2zvnenqlk0c3vasql5cu4nwgxruahx45pp6g9jeuxrnlremueyvh3w9fce7qce84y9r6p9veetxrrlavwqfmsztn02tqzh0043d0l6zp33hntvljlkf4wk3u7tteqkwjhnm7unyu5yna3xywnjq4kktw9y54yejcdmmhehvcxjelj29mdtfy20dcggvy0ndlwj2ytgtz6x2mnavwemrv4plurww8r4537zh7k2ygkyn8j3jq4yrxted39vnk0me9g8rggu7zz5045jp7m0w8329f8reas67xp0vwl6zt7dptndrl3q29tuufxyfa6q7dg9qv8vtwmug3e2jfurjm9qvhkyx33xyta9f8ssfuz2c9fejstr6s38rkpw7jvcjv2nsq6jw96xd2yshz50pxq20dma7d3utz2qg5nlqlpyjc8g8ja7gmcyr8qwl648pggex5rl7xu38upjcltczkusrcxmj06yfxg765m432vgfmn32ahyanh8wtpp3cqp7lypc95eq74p76r43fcld4myl84d8383ms5wrknhru3hvmp5jcauzc84x5u00krkrh2zuz0y3a8jwdu925tsry2vwm3k6vz7wrvupz0m0kgy5udg89cm9rlfjvkgldswvxf0mjv2z8syu4c4faw72f4eh78scvj7wymf4vml4zt9a9vja6mffn53nn74vgtd44nrvl93lf349czzey8uk97car96upa3nx83gsvf54pwemjvfgtf07gx8refk0xu3nsn6ndczvu0e3kkmcauktn4xl2qsrdam9rnrpg9z2vpx79a3q6uay22ujz4qqvddy3qn09jwek375aec8x2x6n9w8thc94ke8xs9g59nt04x07wcwkexs0vst7lcxdflutkx23sq86d7hws3622mny0ukhr93gl879luzhke66yt6su3cww9jg83vp4jk2p3thryxvyafr4ysfcwueuz5yeyc5sm0plzldqcxntnfm3edcdr734laezvdd68u776r3antuzrvkdh99zlzkzaj4gktcu75fll8qrhnkww2a0ywyn3c2k0s4e4z6zjqea3sx9h0a80khghmel8v7kyjye2f54c5k9vhayx0lnlc2dxn8wpdhqnwpp7mx0v99u5cz35st730lc69uu5qz8yp89krceyllkzz6na6whtl5hv09hjcj5xgcwzl2w2eht0judcpgzn3nmwjws4za346ytpfsdk7jd2yyyruvqytr8e65yfxc52qzlkrn4u6q3l4md5d3wfc85hv0mf2z6lp44wfaec8d26t8lzu8jdwyh4ahc85ta9rd50ujp4estv6feszqpdesuu569j2x9460aj9e7l0rpxz7u904r8h4fmtls5r2phte6d7uqh8xv6rmv2r8sjhd3mav6m3u22cyje5w76d3mjmsmsdxx86u0xkents4r7zqlu94hyacr2emel4f8tu6k84x7gxwtgc5f48juj5jdhuuh5q0269c3nyqy2rlc4n4kzv60cj3k3h0vy8enxep82wnu5qjsv80myques0h4322rdhus4x89ylfttt8csskxe2xljnu73ffr5uura80mk7uny89fq6cq8m8sxmyqa7r4p4x3gssxpdmkk4zk4rpd48ajesayll5vr4sc969z2uzhzkyk6vwra2jl9fthyq3feygg2uhmk3xfst7qhstz7rxzlkpd3zpwpz83xst2clwrzkumqvyj70aknhwhs6fl04hufznesyqt32d7nlpm47jggf75qp7640f4vhp5lalnk6natqyftsgt3vm8fu8pzpq0dd5tj0trga03sd9mflznz7tfexeeae88wh4tgpkk2grfhgr7a6nvqdmwa96huh4c6h2x4k8y9zcren82n494tm83gq2ew6sn367z0thxc2r7td94lxcha5xevu0t4sy9c933ksnny8tpkxan7q67qn70mwm88ccc3cz9hhrhppk9dr2whjlz7lqm0sqtm0mlhvxjf2jdzectk2f8dx7nhgspkzktqzz8hyhc4h5p30n73hzfx3e62enljw0nx6sej3k5f83erf40wveuwgkw876h4t77chujrv7cx0wsavueqgc6t2v5jy5h6y63a05wp5cu7skn5np3gkmnyzdegdvg96w4mtefqvyuqpdjd3d75vvsu2ej2hpeqwa0cx6swtxu3d5tuy42688x72dxkpjtx08jdpg0quzl3elzk3k0sf0gkhlvdvzdnw4whllf0qyafdnr8f0rsm84z792q7eh8xvmy407k7yrrhnslg62q06clnxhjmwfy0a63a698svqmu4sz7erfr4jutpekvmqqy23fksjwshecxufqakxjj0dthaplptgg2ugk4u22u0ygm7smjhwxuvc536gdun54eezm9xtntpntfry9j6vxfdemqk3s8a6pjpp4p7x43uear3sk20ajasx7mc59f2gvx3ej0xhu95d25j0vnkxnx6j00tjapqnd2nhz5vwt4gch67mnjnk92ugrd6f2nmkswz05nmngwm49zvyxhtyzwe73e4wlgq7jveu0jzt78jr24j8kkt8zl02qqlcrcxjg8yav5sctz0as4m0j4rsdu3hqzf4jmu4y9cd9zmra2ahkjs9dl95wa3le64k9d6vq3q7t43s0v9r3upug9jzm5wn0zkq99qcn9haq9v38ql28hmxky4rqd39c0ree8r76jyu3kclr259vc8saup6xmeheu", Network::Main).unwrap(), NativeCurrencyAmount::coins(4000)),
            (ReceivingAddress::from_bech32m("nolgam1t2lfgz0wsptunnfknkg5y0lk3gmrzl8hpurt73k8kq6fppj0rq2m0g2srgxtasdd3czgn8xyl3d9ndwwrvc7dq68wuxqrf5tq8kzy8d44lztx72tydd45mg4m4rqqdydv9g44nhadfyev35dgrj2afwq6537n82zhduws6q463fkatkk6k6qe8z5509szd9acj4dd2sfy9g97m5cku6paz8z8uk7kectz3nf0qku9s75uk7yd0qx44x9nxuhg7lu6frkt3rqluhxu5qjl6jj46s5dsnapyqx34t30vzpkkkt3nzxkkmnmmc25z9dcgxst53c6g5atu0hd6a0fdankyf9e7utgym0zewlny50frdqv5d7gca4hv6k3sunscl9tnkwarwt09ld79gkwd2gefm8kdh9clrehll0q5m5r9ecp4rl5an59zzt099jvz8xa9z66y2n3ecavr9gqhm2yv2dpyfl0z5eq9vx84w7phgj54fevm0scvt92vxam3cx85kg8fvea94y9ugndcwlghaue5h7fqk9w5cy2lt0fcq8fsdeq2y2g7quutcfg8c406pfnelzx3chmapw89xum5a2wdrff7tcduw45f4stg4t2gzv4yl23yfw0qh2rgeukq5z9yq20wfhl8f2z4pcpzxvxclwjhfqhw9nvkdwp6f9vdglg39hs7up0ke06pe2rh4ktp9jssqkdtjglps7pete3cdje5gz45ulpnuz0s5w69fpuuv239e3m7qeq5jwz4w6497elmlwrqkwnfn4wsnxsntsrgeq6dxfq0xmnesvqpxqxfn4a06c5k7jdfdyaualqnj80azq0pqjpxpll2w4vjhxatll6r9fxqmed4ycewhusg285fxug2hvfhlhafwc80yw3qmknafkvawhuh43jj2zk9h79qwdseuup62a3puryx2rumzzju9p90rxw7a70han8qg0dgnypwgwmkyvzhls9c4x3eqtm6y6s475e907tn3vt63t8u6sfcw3ftul7y33tlrja43ajxt933lwpwma7zll4uz8wpfzjjegt05sd3mzhay9qc8ntm56drrssahc74lpzpelkxdq0w3ja5fmvxmyqvqhgdxmy70s2k85s0r4qzw85sy75gxq8h4y4qfgzzltpyv2uh38lx6f233dc4gesy5ncpln5fzyur5fk0vte5en6za6mgfhhwa7dl7p2q50pgds0dlu8ul5wltxkcyms5rad49ny8fr2hxsffphaahmx79xe73hglhrqlv32dlkwmkauyjl0ysmh06egaurste6hk5xzp0wxskhejfnx6vnsvzltdcc8qufyqn492l5xa6tprqp7t9xgmrhjqe6e4cdwxde5spmumy56l3lykrjzgahye8dupq0g3cdgtljulcvsyaz6wgmrm30pcr9k07vm98u32d8cyvh8ayfc4wvtcnvdqwuu7pz8l0gz0rsm06ps3c3c2ujkn8l87cctlkwsxugqlhw7xlwecgx6lqssu7uhnnmztlxcxv8xdqk8cf734re57cn3ny0ahvkwr9lsq4zxxh6lxjvmh8yj72h6y85tkzqv6mvy5xyqruelhwjra6c2hh2hhqn3w50qldkf9cgzsjrz9fc6xrtqvhhxuqq0d4vhwjx23qxcsg5z3k9qnpykkuac5qprd5e3uzqlu76mkgwhwr3g9n7gffdda9nf36764mxj72pm2mm5hqh602zyr6zyyuwtfw8h583905a87fksy99z8p7ejm9ldj4626xv4a8nkftccprld2clr8uhw4yyw233x4euh9pe4djp8h37vazrqgwjrayez8ppqmjd8jeycdfevvp748srvpt9659afpmqvq9yqnv8070yrvqa5pnlwfjrwpswa4wklc8tl9ech98ysvpy8g4l99246qjmuqqyqntm9y6sx0zmuwz0fp43y3eu6gcf5yk9r7x9rxz9sfx7mfkkncq7f6rtevypr0f4dk9ewqxr3arqx62z4mwrtjn3e5nuud0f23f432akmad6kftcnrt3g047kmc3ee7dzmmrtxl5epkckvmh79yjevtqdw8vn4pw085n4ylp5pl0mkg0mw9c4n3cf603dxgfydfceqwz66z062f77exxznpaasjw0nxelyncg68c7y94xz6kak95cdm6hm27mr3qd0pv7x3eg7jvlw8r58h4408aa3agj2cfyaxf7lsfzf7955r6eazz9m43vtsgrl7hd9aqps5yvayk5mhsjuqqk8ejdthqu07dpzcasse9tvqh2jylc6k6gw58ls0mkz32r9vqjr3g6r5xtff7wleuw9ffy4lv5uzc9twxa4hhe2am6p0wzezdwkt5ht5s4gnasxnpwgl8f0gaeeu9kna5ufrtmxw9htfc8ruxe3hk9e7tpkpzdzdpqavs2z22p7xs7kys0l7xpe9hn99hvwhvdft8tp3kvlcr4el4a6nfaxx3cguqdmqznku3uvr6xs5czceq5qn4dpstakc2asp4wa7ajkukuw64jn398n0knfyed8cvs4wqdndplz8fm9mv5djzrqakrlxylkkg53e2s2483l9kvs9yrdmk9r7asds47efuzme8yyya3p5nnjx3srfdme3etlvyrn5syp6mmacm2xzcwfkk5rh2vaartp36clp7lvlzg3p7ur4zpt7mx5fyzur5v9j65hwldqu0vx69luk6epq36gyxvkremjkr94s9ljjex90j08kzx7f4450nzjgav0wxm4r3flfwy54lzmnl9xy2zf6xmkmndxgt0d9304snwwnw2p9f6trmya39ef7m5n23wakzdl9x7n2zalggau3wewxkaxmyl4hmf6svfhk09mye2zxy30qjcfz2xjlh6qrt0zpnth8qwmax4jv64qlgx3tycf5aat3ydjsglf0l0jml3dzs9zrhrzcyygutda3r5ysfl5twt7wrkmw3m4c64pvn6l7y35lh9ncjfy7x004ym4u49fn4wrnu5sank3drcesdwe5jlmh64umqk7y924l2yctuvfss2qkcgm8z642u3pwhnn4telz7r5t57feww5fvm2j5jag4vc2z7x3xttqa4wu2jj4awd7vnf86rfakfrwk2z5duhafllmps4rpwq6ucy77vxwkmclsmajgkvtsasgzy46gcx937sdlrwt39c7jyrfjezrasnakxvdy9nfktdrj5rtc7rf74y9x9m7a34s3qkm5qkl3rpgn5n8x07kgnlcaw65snlztntvzq232j7g0j76u5gek4ks3ravg6qdldwdjlwyhf2g7wx7m9pv2zl9pkd8x6t9zlen2w89mrshhsz40hp2jtcgc4x2fcy7f86s28aeh33980gfwes29p04uw46zql2cg5sdzkw8r9n72yv5mdgffzn7uuqvukcddxq32drxhp6e35", Network::Main).unwrap(), NativeCurrencyAmount::coins(836)),
            (ReceivingAddress::from_bech32m("nolgam16udv4kdg87gw0mawrslp2uk2kxsqaqg454jd4lz72fxlvgt22sxxgtjjt4wqhtw0gvgqmavplgqw4t28udl95aljja26yytvh9asjpthqr4fva8gxsrpgg4rhxdspwnx9phjvw9c5g4mxmf0cgy2f97qxsj0m5cvmtknnwgdxvqq03ua4vfugv0d92wn3u9x68zz7nr2f044d2l5xlk9gzyrz59e4fgcq5wtn5j2xm40lnwq4h60ewnl97ht0536pxctm88n4v9y06nlghlfdlzcr0q8py7d6nwtqc5srf0g6cgpazmjkd5pajs32fclasrqzsfpyqhepw4acfmw2jeckwxrdgczdq3xcrz86ypa8ky2d0tzl6fm98m69sfqgqpx6rt99u0cfnvnnaafeuupdrkvyjcyzuu8aznnz69a848h3a628397llr2wdfrlwr6gwvuq82raganlls620zax5nth3ncl7cna00rdss336mazgwfaqnjrtxr98lknzgzr8z47cegnj4qp7gmguc3l4gvuq4gt0eke5apyrjdpp6w8u8cf5lczzxuhj78da6vd69lyszch8a6eyn503q82aerhjzyt8rry9a6zy5mrjrssuf027hqdynwegmq6e0u2d22tgfmst349y6rtu28a5ec56kfztpnvdfus6pelar0cst83u34d2g333xkf79vq6nsqg0t2ey5rgu5c7g784sj0gh0e0vq67xdw9yfe9pl95scgsghyuzq99s3tmx7q0ns5h5ad330jjr9ygxks3nv9rsf75elwwz6aez302lqq5j9gepz5dmc6dfmk3ggx4uapx8xvds96xmpa9cw4pkgfr8n449aepvk7925k00e553zwfsnzxepyy3xgxl5f7g5ecayqsv254x84k0z2fhuhgrvkqs8e8tav3sd2tcy7vkl4cxvqdr0umc8zal5fmyt0psdp0j8duacxd4v2un0xec23wgg3u9qjffpfrtzp4y9rk3japfu282l6nd6dr0jr7muyytpd2yftumrqx9hhz0rr9vwhmmj8qjyhmzwckxd64rhwwthc748332ghjhgt4casvexev6nyl09pnfjh5kvdzf3wjkkrclxu7p7tm7g7y9qkg9uwqgayun9ha9xyc4zjtp8yphqtve6pwjxrfszlmuzxy3c9sn0ffxemtyxr3djfvruxvx7kj6wrecjqelnuflzj7fxrrpzn7javt7hm3sytyydp3la0u9kqhrua7fawr9zmts7lxjpsvfjy73k2an2hlfd3kdw9gy8gl2sy0kw2ft7mgdcv5lsvcprzcmhmhdacpmdrkguuy73psytf73rzst0umz7cdhpphgvchq7shv9zctg9fng0a7tkvxn2x2catpgn4m8s9s30c5la40hkvek6hr8q5zs272843987v26g0aqrut5xg3a6azgfve93rey87hgxgfqw36lrj7ge8aaae6y4pr9anyrvslh3dv3nke00j3a5ehldtyrm707nw9f0darhtzfjfz5g8x5lvgwxcjwlw2pspx6r0kg66ff4htdj9q2t4adnr6kveegmh4xeka0ayetz5td00gsg8xyqy60lmuec5pyg9s86c96yaahnw4cvpdxtlt7r4afuunsz0mnahpxf3qh0ax04qefy85a8hsffzz8yhaywhyacv6wem2m4qqys6gy8z6hx89w5j0xpl6a0qvlfu4qpldqk70p0na9202yptrqfy4322lam52nmvy368pravuk7uy58v6efaavwje9q4k84f9ava2le207tv0q4xe5njxc2dzf3k3ckcp7fgf9rxzkvj0racdaqkjp6r84adlv64n6seyfp0sc7vans5qsx4sz5tu23vy78gvehp96svht4jjk7mdt500l9ud8hq8a6v4pyr55v8q44gdglmg2ks0uctzfehl9067x95c4nqw2zq4lguh3yh4kxtctx89zyy4z25vwknnr23gecymc0rdue9szly8kjeestj79u536ewrfs3ecyj3kkur4eyd8vmcwgh5gd3cc8vzk4l26qz0w8a53thn3vqf2qmng50ckzlxwdqtm3fkye62mc4md4j0jkfdrucncnekt99a94rvsp3kcq5m8stust46xvu8f6n0su336a9wleev3g7ycddmpt6c5jl47jt58ku9l58mv3n0h88x0u8ky02g43zsa4ctk2z0mlape4z8swm9qzhsqxufpps57ypd5d6cq6ef75cuttcsj6ag6595z97emsskqqwy2yex80kll5knuw8da02e8tjjs4xp3jhq2ukrf3u67cq5dq9urxjh3afxtdcx7sw03lylspsrzkxq4c8436s8qmxtnrvhrn5unzd8gpm44q6gj3wq4dwhpq3gd3aez9tygnzl3v054glzsx9xhpsyx4e2hcg045p7gcqlpgsj4p78kdsjwhfvhq72k645ukxxa2t8n4v4cn5agnayras8u4tfdq6rmenayhksayntzcgpnt2x5z8sun0lrvgncla6syet8rfxp5fx5mp4ndnlwuqfu3689y5c57nn8tfu233y7cumqg33vvv898h4dsw5wgkdw2uax7qmfzhp6jc4hw55cx8m6wytkr3cj4ddvqg6ul259ql93gexlyrr9enzhnxa5nsj74ea8dvmhhqkz7ecwgtdn24c7aetsuy6wll74ws39x0r8qfd3p66lsqh9ufamsvqhtt3kqhzaa0ppccva24sz2z8yyg3jrd9eymt4lndjqepcpf5dh7spnsxn4mykvhtyrlr79as6c2w7tt6yrana4hmqhru7pg02597f0y07cpg9krvw0sgktt57h30846cyukme0fw4mgrqhzxherue9phw5x48ep3zlssx9yq03n8acm5hznmt87yu9gznwzsssqlm76hfay2n3mzf54jz7hl2vpzjf82l2v44rvmj9mwyzcyasjy2n9dmk2qf0ejhjpxpsv3m8lqdk209u7f3uj6txx755pahqk3jql3ztt9m92yp55xyhefcxsna575txycmjvh7q520ypckdtnwn85chkp2ltsfpt4ljweqkspt2x2pndl23llckh4awhm3fehhtnncp0t4r27hlzx0cjch3awlv9c6gxjvt3d5f4usfcj0wntvrv2kmjvh7625jfvvcpemdwzvrvcyp467aeyfld2kxtf0r4algxgewn6jgwgcxeps3cygr86lsn6ngsly7tgscx4k0yujk2zxctuhlvd6k34d4dkf7ullqpgpq25mtswmfv65003alujqdz07qpqlqflrs4afh9x2jc2ghd7se8lyta7smvvamtc0z6nhgykwtugwuqrdpxwm8j87vuwsdrgmusrwjjr78kr66j33rw6g6v7gtppwf0ez5pj5vs9ljkcjpgsvh6v7v8whpvz", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam17606eq6cevkmfjae3u0xw8hm4y90nvrfc0e3z0fy0dzfjql9g2ymf0rxrcfmym9kn72r9sslcff7w92gl0kpls4yfnn7k7r89nm8pve32am8cukaetp2kx0rjctvs9ta7m3m3h6xy0u0j5k03arg6edas2szqs2j53ck6xfjl8lp4xq6zz33vqfxnnq3ha2a4wltz3376lq26pph40wf2kfhy40wrrh0fu3wa0em5q20chzpecd4dec2k4zy577am376fdhvnj8g9tfsfsjpy6d8z0mj383mr3kpquhzrvayym6hlecpqedn67d4xqexcr602arddnwajkmyxl4g9s9eteuqsacs5g97rf636yv0uuadw8p68hfxqgp67kaa4djrr9njtqv0umaa6gha7q5fddpr5h65eskmwwq6mdtze8l6a22uqxe9yclrcy8ywu67mczme4df6cxv5r99t50prvgffxua70sweul8fx5aknsparq78fr6yzy87q8cz3p3mkrlywhku8qckuhyql4s7sgc6j3lk8kvun8qpazsa44rwhlwezsyzjtggrlfn4glspqspk6c7ak92e87vmuf27s5uzz936pf6svlpyyvalk24ywdl6vmejfkgq6xrd9v4urpvacrq2g8jvkdrqhlx26czdyelnqx7ww26fv8ess4k4w5sjy46fa575zc4xnmh822dmzwhlx9a9akj94ptda8cjg7zxue7zz6pt48asmg90lykarfvs9m52svu7xjd5yeldkspum9tkqc03eyn83gvf79g4mpdhk7y68svrl4su8rln8wjpe95024n8ft8sxqq2jc7vtp6hu5cxxxahcsfvx42vxreaeq64sq7j9736jthfnfface4v2gnv5cupkdk8ks8vca5khcvmyfd5zg7jahr9f0w0havu5wxdq4yze2q2sd684gxp9xd4ne0m8hh8q803ueah8hrnhr4ccljun3jhrsw7jrshfuxrum6ppukwnanse2w6zevr9ed7n89cfjxg6pfrgnechsfgy7cgesjx4jhc5rj778qnsk4lutpx47ely3u4zhy0hhvlnchetewmgetw7xcqfhjn3cpvjtnh9qsh4ljag252l756gyctk9rghq24c3kp8jrmc30jaw75n0lnn7u7p8ymfn37qc5tapvfk32ca9y3m7fkplmul5x9u8sgh57ge269k5vwkq5ckt5zgk5pwlq8rpg80tarcvwv4w6wksr5uerruf3pdv9ucxuwuxs0et4cpr87uykwcg3aveg5fktswd45x8kfpykktmjr6zq4jjnxge7nwlfe852cx9wdqf2f4s58g2l78saq2pam6wl69mqrdgg9c3gvlxktcvwgfq93rm3jq4y83x8qeksgfhavsqhwzxln5a0v0s86a4htg4w26zztyhz5ul5kyv004drvfg7y6hku5vfy652nq9pnmyr03m33e8z568qhq5e759t5yrreywdh8r3e3jerfa9kkrx97mekhmw6e6pzudruua5w7xjeh5h7yfjk63fqzfm42xdnyhcm8sut7l78a6dza4c2pzcscqh4vh536pvkape5xmm03f5jek8sp04lqw0n23fuhasvtqththarcjcnfdnj8s8rasgcrde2tl6aq68pdqqeyddvnm3p43mleafylm2syfvpcx5vz6upupd8mlqjy2h4e3nwv3kveyfrcg40g4zxt4pve2h3f6nt2gqhm4k5ck0h4wtsww4a962u7r4mdrmpmvd9ywujynvjckdcw4a2kzr4zjjs3ejqqp96arh2vx4nuahe9mzrj9tydgwgurc66sayfscjjce3rageguyerpdwphx43dkuecd7xasckfu88y3z64j2gzsk3wagsqc2c2g683x52agymlpkcmpjcsgl7dtspknkz0l9vk4agq3vtmfqmkldhq4w652zm2r7nqml48e3qkumgtm3tnuvuhx63etnvlzmfu7q7gge8584gr6evpg08333t8u59aj3xj6v2jv0thcv87mlr0n9kty2szzgt7twzq99hw74yz8ed29zq8hlxl6dx4ph9ksawceapkfkn9ucuem742fwepaya3p568620vfz96acfwvz43jxwvatqgmtm9udw7qlfprj5rpkhjzqyyxwccp40s4pkejutjp7a0qyxpps5wzqe6965v2ap5qh7wccawl7h29mz9d3wkzrqdeqkad556fwjptc6zv32y600jud28gv4zwclfs0967x7def07lf9lepac7j88y3k7fhtw7s6n37z7ks09lywdd7ua9yxnqthsetwzrnpeu7ps6vewahu6gxdf4jn7m6z5l45skf47ehgg40udamnlktyq5au2re9xduna6xvdv9j4e0lzuwxx2q68j79vrkhpu4v9s79p5x5upzvg6q4w9qqq7xf40lk6hz037wnngj7azcpz2mtq5zf97a2yd3kqrfa9rc7r9uqe4zegh9lwud7emja5thjhgdx8elen6jy4pwhfu30dnd4lc2muk90kmzwaklfpal528pdtud2zt802zcky53vuak6t2h3ef8a77el5va5mssjja4ly8gul5vsktynlfh3szfntf2ns0kl5tmh8ape6ayppttp870u7daeaqpjatw0sjdvtmg8604jw3dv4mf2s2sr2h2jsmfu6ta8jgw7u7cu72sgamplvycx6fkpdjtlfnwc62ku0uz8ckg8lh28e5tqkrsndlm5hk4l2c3pltwutvzrg7n0z86ag2pd5xs72yu7z8zrzp2uu23vdanhuffe3neeh4u4xehkja0qf3k2fxznmvkglsf34dd8q9p0v33l3ld5lp4vrk7hruzqhs649hw38cdt2agpqta8wt2qalhu6dy8yt9lggvjhdgam0hs7rj38ajkw8mmr8jkz2n4ckunkzts8rsc5k8sy5jp2ky5x8vzq6ru7v7r02mcez5hvm9ap2drsau8ngmex55wa2fem2yc58z3fdpw24rvc3vuydc6ezzdmwa4uev5s2uuev2a2j93pa4dqe286mu0zw2ghdp7xxumx03452ec5yay7l58csrcu4ej33akvcvgam92upw0ku3xp2rpfjdzcrll6uhl7uqht0l48urad3l5ms6fzul7q3ug03x4aqfxd8zna58a2ngpmwyczkzl3e3n6vplhf0aack0sdu5vmy66utdc73xv2t75hmaxf9recudfctmq7rnc4cgvz8cwctek08dwrlky885em8lg8cq0wgxqj3vs2z9dthzukq0ww0vc0ha8vcaqqqegn6pqr5yzvmjyckqfrsa876fvhztwk0yk3zm7ez60sgffrpn2l6s5mrgmvn7s9x4s88uhxfp4yfeq9kdflxg7ra9p7rqjkr4e82xukr53f69eag39p705d9uwwfppcy6", Network::Main).unwrap(), NativeCurrencyAmount::coins(2250)),
            (ReceivingAddress::from_bech32m("nolgam13hxnlux5t8jwgqx4hzajsmj4wnch06h7l3aafwfghf8whzjese5cx4splqqyzuy35hu4ye9pwrdvdymfuwmcdf3hseqne0k6scq2wdr3xk2u7dt768wz9m5qt44eg5fl5mrmhx0tc7z8r9545893leskpmhrgyax2mkm7dvccvrd3n4q89rxk4356usls86dcrjmdrrl503y27qqfhh7ult4hxx57a88d0fhha5hwp8wh9cwq9l44mrd7ddqttu8hx8gzsyrz8pe660vewdv78qugeptra7x3l3akxffdlcqvmfy20xtpt5ady79sjrfehgdy3r7jtfnd6x9n0v589p9v8nqqjke9yvx4umy9gtyzgt3pxq67etxm443vqxkzrnw8v0t9k2x7ndfgxgvuzfgf9qwk6ur2s7amadfyqh5tppz5t09ruvwlpp89wu7fzr029dwxsrn2ghly62jsg5fx4d4euujs2ax9jlauertdg0s3yy7zsczeuzzsyaelsrg833zqyd355mqc8htxx720ytgr57jx3ph8240l27qn9ycphgjr3yh53vywv0x65nwxa5c54tannws56qnec7h3ud08ltdh5py87su3q0h6t676ykw56w7sudfqw4plku9fk5d7vc60x9m826s90t7wetqfev9jhy59e4ysqwz30af5eqmcn2er6zv83kh9kjdeg0j9n6aqc3am3ksr6p8qnnk8zrz79aj8q73k0zl645qzz040h07mm9dyxjs8tc35y290cq8sm86pjfnswwqjeql2v00jy4srkyccm3k0rqarpmhylwn09jwa6jjtlq7m4q5a7la8727y9akt8lnldgy6tjmxdy3jtxpj90z06qt43fpu8jj9jyzywey6g4jffmu5lytewpv7a035580kad55lx5at4tjefvfklq0q4jqw0fnvnwvrvj75eu8ntn7j6hpjnf4yuansgecwxh8ecfd68lmjp42d43rzxyj26e9yrm0mmzmuttgmnajnktdtla3t75zzmn4eahkkkdka40u9c9kqw6739jhhy6zluu0e8vwm30h0jf734khh49n9mxxfad9cmtug33dtag723q2sj0gxkgd6nuvj88r7hw309n2yrswaxvcall4zd565dzmc3632r6x27tv29vzxzgkgul2657l4uywcswrtcsncerytx4t00nj8699083xapuhck6sdgawsudaf5wzxzusrem7vdukv0m24lltehx6fqx9uqqnmpsswuv7hvm2ar5c908u9c66hhm6sv7su2ny23hah9pqc6y9ujg6pt0mzqa6mg6qnkk5e2p689zcc3w396602g92teqeyx93kf5yaqy3ynkags2t4lwtv9l6n4s83zselxlfwdz3l5jkqn6tmkh069c4pxkk3ctsjc8pg54k8vlag778sdc0ck0ml5r2uemcqnxmlvuq7zrdtdgaqptsdz4np8rfpaqa4lt7xe52qppzs3uk5fcmpsmnlc5wjj7d2dr8ar6a77xxrhckljc5e6xe5gkgt5dhg9tjgnr7lcpsychq68ff4s534dwqkadk4hjqmrqkcmf37nftngqe9vvr6a9nr9excuy9n43kmdrkn90mf4u6m56nmlj82a5ppldaq3wvfhxlw7wucrttwtjaa9u83jfz3v568rzhyuxnqje9pux7gt9zcux6y0pg8kpz34d2te3vdhg4cpjcuppk8w2md84xn80sy3qj2dfh9vylgf89wej48qep22vk5k9l845eht3y4dh56f6q6nypavwyz05vucp6qyg6z2atqkyfhvmqx2h4ckwv800hwst7gk2ag29fvxtz8quum7vy6m3cu705tn8m5wy6czl93k8sk6cj8g80pa6urxwpu7qj83cvfqvegprcm8rpnus9tcezpmwmfgkxqhzauj65usym7apjx6mu9q99w7pnv4khwn7h5ucdhgfqw202dknhf3kt94x3uq6jwe8x79sh8jpak0spmqffjjzw5flk0m4kf9slvsuevfdr362d43vcqh83uda8whegv4cadhgzdj3h35xq70plwe5ft5m68zcrfy527syhmfewme50xdc4prl5q5z3vaxl4n5kg4604pq9cjjzpr3t8l4sqafsshjgg4vdtrl80xac77xek4y5l2upf05d5p9n0x853z7sddr3an23qxgs48yld8n77rr7mfssjwe4wyp0ezvh5v84cad2r0h54425g8dve9nq5lwklvcew6e7edsl3u3gxlhazu376wzqtj8h9s2jgm5rcz7zeutpt8g7qmxvcurayrg92hhcz09pvkm3fe53y79ypayysa7q6tqm592l098d8h5xa3ewp0s8nq8jm2e3ruqe74wwk37047mp68uu7c9xarm20qur7wl75m023yxzvhycdv5snpumywjn5qm854dzsy8yse4z54pmzpskaqdrdk3wyzkrh7z3we2pp9zcs7mr7dz9604p6d38020q580an0ls9c9xgv65vprmy3efqmterjdsn9wsmrua05kg8240ar08rg2lncp9zjhsu3xjfar3cjgrfmwep3qf3ntt95aa0j2gn3s0l99txpdawl2jgymgqntckypcqpgpndjcwlvfrnsq04kepjzc7z3lg08rav599u2nqtdaguga938l5jhgp874k84u4ygwrzwy2anj6lls4wles570ksxr8y5d3u4xyjxqayk7kh39td8wemtrv59302fm2lea09undd4yda9e3sw06de7m6792mp5sw523ll4jmvhx7mt05mhl8le43jrjm6kv7na2ar8elyp9az6r02lhdtyfm2vy8m7knz45w5kxzj3r5uwffhal9q4s902wud0mr7lpns4h798e8r2l83qqvytvdy2qdk0kxjt67zjh2lhfzghc384vrcdntvcyuw4j8tder0v7ee0s3qhn0mmxf9p2yxtejmahkpqkfhe4kq2904w9jqy0fnssk0mjlydpp6h4zlp3ex3wg29daqedat3x8hyxh6354twadtzy8d6dmjv2gdxfwrtxylcmat0n5cd4ykd6uywnmr4jg5zed6tw0vhk5vh3z204q7aukl6ffup47qfg4axwa06c8et6vpee2myudjfan8nrmaymcm4uu8suwt82090mwy89hvlwdkz6kvt68m3eqegstyjft4v6n32ml8m2ld3dd34gfzefp3sgzfgsc3ztr4ua9qg9qj2y0gjhlj7wx0vw5h34848qck73rcheuw7m7mt2p9qm3x2mn6stmr6wdpm27uatvm6wyycse6fa7z94dclzvey7t8h5phr32ueq0d5l3tnuuxhfhssn952kmkjnpxt7ghk7rm5s5hdnjm7yn55n795qpst28q6dppke99k88szpgtu2rgly04l0h6trvhsujy57v", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1hdmgyx2qgvfxh7tsah06degmajqn4p7f8l8g8l228qukcc5f7jywnzwnj9yxejpr3644mdrp05njdv8refse6z38k0jp2w86xfh7lqxvzy9uqq8aeenttpngzh82mwez543n43dfjc822fynj9yw2mdr7nrp8sx332txle2prj4yp86wa5myfv2rzsth9yz6kgzfzhluut9kqzvcse0a8yadhh9lw6s74g2mefulj6vzplmqvtr97mlre5kxmtl2xczmkwap2eqfmhd4xst84xv2atdytc09sljy696wun3a5racxpdlyuvmllysaraezas8vs7d34jyqhdp4cyzt9zmrlxjy2s6mn056dnh40fh9gg6de0kf54us0vnwu3xhe75as2dldmy532k6cy5s9mpxwh5m6zl0fyhngx4v2rx3575gtqqzvrzmt29h9dgh9rlxq7uwqlfc3ts5mlpd3fazqtwhtemfulaaupn78tqgmrhe9unek5ky3gvsapxuak02d23t9yep4k2umxtlckpmpxyd7axqxxgcrn970p24lzq522zn9arrw6tqpl4esvxn5mup5lpkpwscu7tlws0j9t3k7swtvlsku5dtjz8v8gtryggs27rzmlsgrczaauwkmuzzjk5ex9jg52z7q80j7qtf0vfn55ve6g3zu0rswkjvaqqcyrljr9t67z8g4anyseeashmaj62u36cmkytpa7ndhqf3u06x4ptgaqj2jh7gckn9seh2d4nytx4trxjpxj5fhtkxp0hjuht5wnyd8kn40drvfkp7m472xm3mugppjce9kqah0yw645430mmjc6ucv4gq2dyuzfjktppc2kj9a4t7kgs36e889w9kkdwfwdq2tnhdv5t082trqvgrj8f0ygjpjrp6zcecf959rerk7vk3ulvcvsszrggm0wn5qjj5ayjqr92spxu9vndcrddjs2t9f3wcjka8tny722fq06uq8csjhqnj5mqv7wkzd8p5chl3lpl5z7465d99kk5gjzrrj64wy3gss7fsfrtkyskg75ea9pqlwm8rn2glsvadpl6t4f5a36lv5y0f6dgu7ek20yyyha3j5pjg0k49ur5z67d8x7c4s6akwgar8e2d59x6m9pwqvwvrsk8lwywnw0hlkpntmhvld37jdwlcnscqvjg7cxt98n6tsaxh56rzkv20uzy2sg5dp2scxu0k84ucwxvafsnd3y5d6kntml9cs0w7l8kfgukpev47ays4h9un8gmez0q75qs62xd0ag2z49dztryvqn62m292wmd3nprln9j0k0mpfqlm535pxzx6t4snhtgs66waypdfxwje5v7339fwz4vpshjpsfyfux8vke957nwup5r0mlv34kkwacn0t0plp6t98x7mdtnvdq69q4mk5l646yq05ugp5v9kl45sztcz30ju0r7fku53zsc5xq5kpxjc2q4t66ckkhe9ec9hmsppa3y2hhvqn0rr6m0n53cuzv5ucg2m8kfxtd5fhtd9jdzmsyjgqsasklxm0eutklwp6ynqs7yfy9m7ltknywt7aynftc780w40quwe3s02jtktu3q60r4gklff6n2chtsg4g20qch4puuwgs63dd074sd36ejhhdnmek9dpyg4hc6ftp22enkmll8zs9ffg6fcmpqxzet3wr6fujy3ctn6desuh9nalsakj9pyyshgj3dfkehucl3e8u6y6m2klz5lv0x6hj0ahrdzc240mmnvrfdy886yt6z7yzgpjqla78njhmzn3ymt4lls4l43s887rse7jcj5u6gh5td0a4zsl0zxrgjhtma6gskswk6ld0n292n6dvawg5f03f7ehac5y63tr30vvq8wk7fhd7h2a075p8rjpdy7n57ed54aw0z4ccl9jde29hty042wh9vz83yxsnw9dfavr5pv9mcvt5qcfp3qfkfez59wkvqeskeafp50xfjjsp6gqx3ky36rgt8e3whp86adq6hyj2c7essmg0sqvcprphhjacsavfggfqcpd56zvfwpz0qmrpj5yqp2kfwt74p69ug0875r3sgur93qnzl8648mdj97rp2ztlury9mt02shr4f5l2cjkxqq7jlxeqrec6dcup9gtt0srxts0ryvxry7t6wv8rsncjjcfvy6dnnetcpyekv2kj878d7wmcy4c9p8h54el6d4rnarucam959r4ht9s9e3fpva04hjnuxk45nhxlyv3hk8p5y5p383dkhyn6game32hqy5vta347w8jvvuxhqq62qtcwrmtfng0dcgwegppm0zdlalh7l2ewg5x69zfzsuccyeuh7zdcy69uhvfh0xufg2m7gluf4077u35hw9r0dw7wexgt5tcmyzk5fd2cxtmjdd7nzzt7uu6c76cnlcgm5yn66twnsad272ukrtlsd2puenema6mtqd07dwyqc034kzd9fvudn8qj43652xdyqnwz8qqk5wtck9t8y5gevv5g94g9rll2rpctrv2u8mlqwrv7xdevdeq4wp32uz59fz6lw0tqjyntpfrd3nja89egq9lf28pxlcjtwx835xhdtewvhpuztpge2urxeuskfc5dt6w3f8h4gxs8r6pdvup0s5py9wmys9w25c7lk8hsvyqvgmxvnzsavukslh7quenktapln3q4t098v37zhkye7wa63qv0e5d9hm9p0zs44lu8tfjl6a6yqsyzkysy80wzpp8nwgp3ztfd7e6vjl0yrn4nx4jgljwzm7h44qn35c76kewprl3m4v3jwdd00gqa6pdnpfr4jtqlmyqqk7tzkn5wegcj6kj2d9vrkaetp9nzvle5f46k8nau9ttn70l6mj2jrevrne2jkstfmqyr8e7ffkcup2v9jhlcmscn6mr0va9pc9adyqpzt4mqa8ahkwccvcr6384nwwzpe8qtxmn94g56w9mkhgcwpr28wfq0wfxw0ywzry06q8f0alu0q0xkwjt8qxs6yf6kdwxknmjm6y0qutc7982paxpymcw05mweg3x7a0yv8xeccckde4wmxp5ntvjpzaxfga2cfdh345uzpc8hjzm0ca3m4arjj9r7lxq62gj0xewv3hm8y9dsecqy26980kg75az0zgsrtwhhzezuw03n7tz54axjzzfnzq43zjpnea8nm95spwfpscu9xruk3gu60wyeq08rxn77qkjy8rjkglrq4sc2v09zqg8zscd6urcpzldlny4w4sgcqne62yxhx2gxx345g8d8hgam0g5nza47z4xqmy77u3cqlyhf47m0sj4jwtzqfdh28recd8632vdeayhtlnpqj73zq8s44xake5k2dfs2lwamyx4pf4gvv2hyhp7kez54kavr2uj9tr5j6eul3j4q6qsxpz6h0lp63vs3yqwud6tm5h2j08axnrxhxjzysrx", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam14laryyfdhfm9xv2jujh85gjsdaju3zaf8xjdt4fszgp2ud4yktp8rczqnc9yaq60tnl4afm7vrm7cqwxzlqvtez4yjecqejrlzz7ac5srj28l4guxx5k823yeh9c4wsjveq32k4t56j007mw52qup2ggzf3aqedhhvplrelj0gvs5c00uat65djsdtakttnee8azw60vd87nrqlqywuar9t5zr43uhc05uxwrfq54n20h3c4atr89ypulvx5dl2hh5htmktsu6k8c99uq7jp3qjr9mu7fxhye8jucgutmev98hw0lnjy2jc8g6exda6xfelsyhuqq5vdj8tz9ltlpyh4ke2y9vzdqfrsvvh6qruscq5dh7g5fkuylfc6tvu8nrefshmmzfxehx48w0hj0zhm2pmyhlunlm5xref648jy8vzay5hyrqzl3gu422ttqjmpqs99n9mdl7pprcsxt09xpf550qvzpqjlzmd2rzlw0s2249x8n30lshsnl4vq3nu744p9rvcmh7ezxzwt6hd3mvzpc3cjzccwjaqff7g98kqnl7yhuqufzkhs325s0qzrfyjgcpam2tl3nurqh6dc4yhcy63c3lgfeygtew97wna2ut3prj3hetlvg028w3aku7afse9um0zyp7qp0ezxh74nfahl688lep0ln3pfkptrsk94equu2akdq09tjzm0mudf6cj2tvqaqqq5c225twqqewf52w2h6y2wr2fnual2rdqccp3rsgug743sd80ztrgs9cpg3k03g9vldn45d80zrc73rwaw3rrx0xmeyqjdldc36309gg6p4vennj35uhsj0hmxmc3rcefuhyexyj5t3nhjh29qq7xksa3pjad9jynnnfun34st43sms5kzx24whn35skjkvpv20p4wv8s6yj0f75f2wlw5ft0ysev54tphq0q4rs8v7m20fwpdtrem7ad3qchvajsc8ux5t597t62vjdzna74r6nx3jwchp9l038y7hdlgls7x633ygvu8v4l22uej6cl9vy2wy5cpndzda4mnuuyw7ahzj532pwmt309l2sl98w47h6hwdnsje9kfldag3l3l6jl0pj46lar262ch9zs2nepqpwe5qtx45v8yd42k66f9swy3r5pexyuux9xmdaltjy0z2v95vzhcush5vqg2ep2ehxzh535hz6gzjd2whh9we9rav7esecw3vmcggpvns0kfmfmm7u5pqwv07cgyuuxhrnuc0452mh3hvkgzykedwxtjwctmelqszumthtdh2u2ynf3jtk0r0v867jcsrhxxhz98ltqjzkvjz0mpsetlfan32ckenl70cjthvvw676lp0xw2n83hrdypers7hfa4r8mqn2zam3m8j5ks8dtrylgyuh86d9qec2war3mk4f4x5uq3uqp8ezujyzc8kx404zsdm3ch4hmexn60zjxuy28mcm2rgctyle222hfnp0q8ls7tgkuhf26mqptfmv4c94rhgkyl3rdxnh3eh50cg98haswdww089ldd49wmtenk038waelgtwt6wffrvr0dh8wyqqkvl5azl2eq8nf9caeggezxngukqcvry67g8hrfuy8sueenqjxyhmtff0lzmypd6f0aawnrmuf54ttpqtp0fuwn4nx6wnzv0ujpnzf908easl7am562sggmjv3ep9v48df9pqh4gjwl7x4v04hm8qrn65rem2r6sgavx5pgtgqg89ka4lmremz6e2p8l0nu6whgep20keeplxsqk7a632ctx5dfnql0da3km0f7th8gsgx5zhpfhqz8g0wgfkjfxpyy8209prhqpvhgleyk0g6g0wajxg02x5krtu7yjl93ct8cd7c3f5052hdlp65uv3r646srqvvglrrtjaajvgcc60dyp3srhrqmplqr0cpngr0almga95xv3ah3dsgmeq7ca0jn4ha8gmpvfshapdscj2hkrrxazy4tz0zu0qyvmwudnxvs5k42ze0y5jfp3tdjaxuf7hn8xlavkqdvywspt5vn3hs26qgq89va6dw6jkzz7hgwcnwq8h5gpuj94xadnvck4xlgsguxvcdatukrxqgfjfmlst5wf273yr0d37qmfsrcgkpfk43rpdwc0t0lxuf36tpyg3kuxauhhqj79uypqpel4zvxnyk9cz7guuxsnakfyrff0mzgzu7dh95rupjdaxflt3k3xuqm9pe94r3k6f7fxnga0syk8fx228rmmsc74fhl0t330xe0p6zgp8h3uuwhz3qd9zk9mzg23quyjrsx9c8mmcfzd9eajlkkan0mddvrv4md8hmwjem26df6rggqwvkfuf5y8424e0l9dje9wyn2rwx3lzp3kny3rk6y0de0wtd4n8e6mx5sewee8dyjamvtpw9fn8xyzd2y8uarw90uwt3yjjsreegnvdn6xck30kmvd09h2s72607qm4329jwt4nm0nap6yur7t3n5c3h6c7jft7fc84d0tfutxtfvwtukdfe0rj0zgdkggj6gd5atps8r7r8m7eet4cl2ju85kjpeawuxhmjkgrgckd4hrz7a8xygzdg2cunwrc4td5yzshffr8d3uqj80ml9sjmurcwffv7j5j8pcs4jq7y4072eut6v95lhk5rpv5098rqlvj927cxthwjs3q50lkl7m0kw4w9a0klwgzhfq324pf8d88dx2n9arcrwu80a3z4nj7mf7sy4ad26hzdarrgmf5uz5tpp34kn0vh8cvzsejf66ssgletzt5978n58vskcpkjp8qnrxpuwp9c845ypwekxqdxsg37576yum0rkqkhl2kyxeswlpzeszqlljryey3xj6yx43kel424vhrhhtl4kl2ew28tgst6h5f72wp9as9qj2am9aj65fe56xe7p24n87hy9rlugdk0qm7tm5hr2pw45sauf6xl0shn8hkg57p84hqqxy97xarkl344mu606xcna74tv25lan77xn3c2urhdxjh29qfxgj9ypc00ffq2kdnesvtytdd4lyyggnt3t7rvfj9cznrp4kg748wwwrk7xegsmpf04m560w4ndmv7qlxm6gexddguggqn3exv9ndcmkunc303ntpqzxxk9r55cxwg9u7kanylyl8lez3d7qltrmhh95djh94e4m93ds8v3u3nqr0yn3vuvy75tf8keq9u4dlnjf38wmwvxzgcnxwyrszr9wdr2u5g9ukg0py5n9p3v69ykw89zf2fajz492gx9esnrtj9mxwykj4gx4q2fchzgh7dlhrwmkavxt4whkznr70cshc2ye2dtg5awrfr7jh4lf3g8yxuyqx6zrn83yjaasr0tjatmkn8vzz3z4397tdxul0vhr4alswtda2pfjplcmtjkuhkrhfx5nda9f8rw0djfc9f9kkzf6qf50qj6nptddf", Network::Main).unwrap(), NativeCurrencyAmount::coins(479)),
            (ReceivingAddress::from_bech32m("nolgam1ycy37dpmjcxhcwt99pfk92jqld3kalk55nxmk8sarlxmk54lxyvgepkjx89nkunhwqs60lvj904j43sr3v5dv2sx37n20m5422wzwkmh85vlnu9e7dug3papdhuau853kn9l3lzewm86h0q0ffg3lflxw2w25kyw6tmsvvj79zzztkg25ham9t0j3udz2w808d7faxs7m22tp6xctnm370d89vgkvwnn5vu8l0fedru2m8aaqat0vn0pa72h9fdrund4jsc0urfd2a5tca8hftmg7fkfqrxktvsup54vqw3ndqs9d9t89xcdg3vg03nefapcwsd3samf4znkzcs0xmch6th5y7vlyhu6hwmz49hnxgvl9xxtgljm0s0l7y4g97amt3maq2ak3a0r5cmrkhn8ke4007m3xwnsq958twtlg79qnc9j70chq7dujnm8ekaxmtrj55ers6weny37z0lg79rn9g4g8v2ytwta04yuajn8tg9cvpufp8lctjgrf3ngylcy6yua63wvuzp7lqkg4uv054ucqszx23lgh5gh25pedrax7pwr0ux3lhpkpudu29k8r87t8xfsgr2jj0lm5ltwzv98vg95fr26l5k3afg74mzp99g2896q7ggpe8yv8v5z7d39qn5yaa0ms9qnw2w6znsz0zavdksfex4vpypmpseflkzrc7flr96q4ku95ka2mvu6xxehnfpe0tutux0sztf003hglkc88hlkfzavavk6h85jxg5ed86ucczpwgjugw0wf3cd3jkrqqe9ad8p7l8a7l4pyqj4fydgnnnuv4pfvx7fe5fth8gcs480qw6p4fyzmm7gg4grjp8dxrpt7yddalv5z604uf0jdgm5cw5susrvtyj92jnhnjam00ugrs9f4h42h77047atg5s3zygakl6vak82e7f0d6xldtl92cmsmqp05z5fvu3f9rw3xcugj6hums2q6dr42uz0ec8we8pp5ve8j05hrz58xyzt0uz4ce6y2yaxuv92pj8suey98xk3kjpx72sj297x54zrqncvxgy3f7926k77hkdgg2yfa2shts5drz9f9dlkktyn4t4srh2edrtdkhkdc8gg203yef9l3320rhrtqz9j4rf0sxtyz8cnvza6w9mvwpsaru7vh5ke8e22xyezs82ue0dc3j02qjfdawfyhhdqrcrdmvaxy8t2lczhw0zpghd4sj8s9gn544uvamftg6ws698xwv99lmsf0vvv5zlnkpx3z08j03n26j0e354n2f709vv94x0ktk6gye2674fxhvuw320nw3gz8gx2dgehm7u5n5tyl4amax9rrycdflzvafthscc8dxxx2sgat074ywk3qs2ryduh8stnzwu57s2k9845hflace8t5wzhyfck3kntl6a08w9k8xm0rla6rg90vjy37lxnxqw59qpc0mrk0vftzzu7j0xfara4kxes2kyexnvg54qqs58rayw0vac6qmc25rv56mphn43ae6uu6ksu0fg6my24qkhp66quyaf8lul4ga4t74wq8lanc56dragf7jj5va4pt4pj5r47c6q0hwspcdqr5nl3d2wyswm800wv8s2at9jzzlpncdqu6uf7qq4fwty0vwmcl2eugl3admh5wlcrcc6j0jmhelrq4zt9d22qtwxukhw8f9qfryy4cyckvcm5fl2h6ttrx62d8h96cj6ghpnq0a4lmpy6uzt3n9atvnn6a2u5anpm3jm7mg0h0lunzylzgpqknvv58qv2tdxw3utqsnwr86f4d5695vz3mrmscrmqsneamgqmy84ethuj2ggms7m7xetvy4a47z7dnmtxw0wapuhc03286x33xw7rjjfelfhef36ql26h9zj9kelt6ra7eksg2ld0tawhls4d6hczhfcnqaj4aryq5qxxxkc7azmqk3dcfgcp7k03wyx5kvyqy6ryqt6u6kux6sh69nmxsmkdpq39xjxa6t06u6t54sqmtsl5hdky25dtzk943maygav5k66c444a5llxj3s4j3f2q0zz29csstcvv58eatzazj5vvhjuyxzqr5s5jxyx04yc4g7xt8g4t0ev9w7w2zwyvf2nhvt37zqttnser50aq6v42lsfgmpajsjf5w5yk6g6vmvcuhdgwdv4x8qpjppp9sh35zzkqtn04vymh0pktkytxt7wesanlfgntmqwr5hz9k8atgluf0ltqwdf7g4k00sq9wr6yn9eshtk3h06tzt493wkd2xal07e63c65zwz3gejvcfwvuheshhswzypywkgsjptw0q7ls5ucq4yqcjlduzgv2xfav8xw8l5jghck3en7k9nm4cp82agjn37qnu4l6t3chc477ft0fkm2a2zhe9uwz2xlp0jfxxztfenxghr8952paw70mjg8h6q3htzwwpyj8gfzrmwp4wcl6rl2vyqmswgfqnwuyu3aj27q7mwqx3f6f8plu02fa58geyzm9aja4y84yn8zkrd4l9khvylh96xjsadsc579nwy9mkm2lldl3827wmusw2gzxrusug54zrzde4mt7ww0c3qef054yervqtsyapuaehta08a2c4sm022g73pv705hp0eez6dj86xjqza0zs92u6mcqg6ssdcm934twwn6ye3pwefthxe0thegfyzqd6ratv0qqpn78k2k7vrq9zj2vcucldvzmunr3p7argw0ednnjz9tv9dqlsn9zakge6ykxf5chvdax4p9la87e50g0k9fhggj0rvdfsy0hsv6ew7hq8tg56dqeus827kzyp96cnu2ek2ezlfad0a56j3wkxr7vntssf7dafgk7whjrp9p4df35juzc3q3jstdf47h0xndzk79dnk85de9ntp9w9xwz6elhzxslszv6jdv969hxetmr3hp7aw69krjz2cwlml6nqc5lffe2d7czg6pvq00tkg8d6j3greajvt460dlv7ck6mrm8lxtm3g082e5uw6gh4lugps4mhank6azt8x468vyl0gw35phyrfxg95dvxw3dt9phvxyxzg88p04tqyykk5pu226jxgz8a7cwx6d8jj0gka5fllcvxnd4e4mu7wsmh2xdsl8s37c65rx2ygjmf98hzzmj05ygy3n6s668fnt8jtyr2z39y5wj0pvh4zl7qmdtjfnu3jyfvh40fpj7hsutr620cntjscyem4nl3qp6zh3prp4nrm6wycuymp9sgxha8x7ur4qdxlnv8ep6e6amhd5mnaylt9qxu38r503u3xfeheqxxhma2vypuk7aatayv0uhpavcfdv0df60t2jmpfxd9msx9uhycauruaqaw5r5ut2r9evp7alh9js5dl5mhe8m7hjdd2pg0cgwgf054f504825qakq4wk6dqs56yntt9yr5yesem5h4mrp9fhcmlha7cjareyzd", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam1ls3998z7n7ller6ep2k33eddqaw9h82h7ylmxsx46csdawm5txnqfl823akl9csdgg8wwdqahcn2k5jkcy79s23653aappy9g8cgjvxnm7eaecm8pglvdd79klr4n0ht750qxmtjj959kz7ncp77qacwej2a5vptd442a7p0sdh24dyaq8t90sy7t9rr5y2jvmzgy7y8t99u0z32lavqcxprehvxrhckkq20l8a5qa9qujkqxu33rh8p2kddx2dwslcd94444geh9wktamghfv7q5fcdahsax4mz6cksg0xhgd82eak8vatvjhvg87ltedr63l9xyz6axwr8az6tvytytkyqu2jgsd5ty73e94xr5u5dfcut4pfx225mvvra9s6lycc73uqr458y9n45nfgscw7u30mgez7xdnrasp0679jcxlz7y5m5uqfrx5tqnqltp04sxwnd7nexd8a4dnspc399l2fpuq6rdxt7f695x5t8syuklr09xguzjmjvn74e5adv2tz8gvz08zd4m0yntj3wuuqsq64np30vl7e07twstuxw5mfkt4uygc7jqpdjvfj9nx0pvjhe8pm7uaggy0mszdmgdpr46un7v80p8dtjhswpz8g5yysrmlcyu7d3kenchttsg60juhy0v75jwfeh7nf36spv0e5ledfxnfnhlheygn5yd6gye2ej90pagnehwukrvzpxquwwn3hzpt8cesfrulew995v7gnrd6c6ekftzeaj9sd5mfks5wgtc26rqfr9cxfyktuz8nfugm4je06hv4rtttda9mcpl4u54gjz0efz9er9skzc4mnnn8vcpvf078cypmjmgn5petjuzxk22snqjyprf93ltewt0p06g2zcqyy9w268frckks98asxt7t6gc6y2z67yccsjvvmde2qwha448kx7t84x59sek0pl4l7vnc3a9sunj9vu8hhtc6z786e0m7a84xgs73duhlcv9kgvurgp3rgaqwgaqzu20yerz75dakwx7az8773y0f5x0cunu9e8e6s0va66hlglaf53nq9nwc9x6zxqpv92h5tr6afngx3705z60074kfuxj5v4chx9qk7m9m9ylfefxu9x4g2vz9386ay7js54jaql8kk65atpr23anch8u0n6gu448ldff43jgzq3xu5gkk6czkc60pg43a09e8xkqcc6s5wy0hywtq3d0xjhp3rfkq0a77h75skgayln9sv70skpv75s4vvcgyf59eqcpf2vlun5judtw3e4myr5gqur4zzjplr5anccnascsj6tst43ffhmdcwvh2hnaj7tsadgakshjtq7pe2p2sf5th9x0822ndazgyz95ck6epas2nez768jzm6ga03m2jyga60uljk9ml4vj5zn86nuruqp68x8l6mayj6z6mqrq83z94v0h7kl39nnypphknyzl9dj725cc3uudg5dagsygl5yrg7gddz4dgtgycndrzsjh234yc2h42q7r4r3vwupt99l7khl4gtq64ppnjz8p06cwmefslvklj7cmvyfaap7fx0hqdw64jjvrwd25tuh4aa4x06jm5rkp94e6uh872mdq44085vr9sdlfpegn0j3dtjm8ftmtprp7er43mjsgt6e4dm5f7mnptjw6m9awqsr7nrz5nprdlrgef3nh4zglrk9lpfanwmcwnylc20exdksml8e8l6vcxd3fmmssm4nc89wqg2a9pftw0ysp9pdhsdwaqsvuldf0uf0zg4hgcgeuu5tsj3849efc4hudc4galx3vh2fq6hzs884sth8wqf0rsq6fqpmle8wkpn75wqxfldj2zddm0pdhcwljh6zc0mvtk8vy2qaukyctfwxcx6q7ev787f82cuzxa535q46p5dvdzc9q29venlh4xcrac69prp7gk4nf6x0e3l38sl9w27tfan2fr5qs0f8lmcfs4tldvsn4s49hy5qrtuev5r86m4rg9ft22gtnu772hygq30tghda5lpyzk8c0pg5am06ayxqnh6ux3jj2j8e9g4hhuuny72zg5cwzyqze2zuerz58xa4suyd82ddav80wvuswfz4f56gxr0rv8hcvegwep2y0043fu3mchsv504v66s24vlpxqfyx43jgysmuyn7s3pzxw692mz24shgzmp7cxuh0sxe64v9r9gxrsjdnq4gjq59fk8382pjy9nl66smc4d5awkdkumme3glfk43khx87g3c8hvhe7g39hxu6602vyyqpjxnv43m9vk2nrsygqkevhw508jrl03y2xyj8rr70hs9mdhm96mku0mvshrcfw9da3zmqel2q0pr6va3nc5e7svvfxld5uhycsgmxdvclpsyu93ch3jfggkmsm4nlj4pq0fp4tt6pxnsx45ed7ggfaau9j553nvsatc2r092avt6wuur5vnq6r9r2wz0yry4grux83x3z8k5s3acmd7pgh2728tyc78dl9r4njw99jcjlrq2az7mja0eu8tx6fpxd3h3kq8uk9hd3gc56a20gc4welrh0whd2utz2wp8p66yuln3y4jz7ux9ds9c760tvgeyfhzvm7wv0lkjzpqvmc2xccymp34f9h6e2d4rmvk2nv5g7uqe75uc2mamhxwz05z0usv8cx56w2dkadmrxsfgkzcm3z9rgn25fm2untw05xklfrnffvas5e7lxa6kkqteutcsw3kk8d3qy80mejlsw4fdjn2wt0gflr9gkgzk5lgtjfjt7scmsphh6t8yjkfvfp9rr5kh5elzft2zf2enqpp5kvn0tyrhrajxv3vyxnelzrrv3trmuz2vfelvnusq8c2y5ynuy08a47a3npch6xtmmf6kuklg6nzqx28njkw3c3myf66mexhu26cazg47e85t7knhepwfgwewfuejygvczjnnxe5kgetf328cwatwmwv9jusulk88uv72lmvfw26peysqh9qjcjmtjtdcvhsn9m3zjtpyfkke3g4jnv0cezj7ev7443q9yz2hrugv5v272y47krz3aahzw2640c2g6zglx0eygs5knhj82e0x405dae0s4p88lyfvpxra7p0tskac75qslr4e4kjv03axn5fs942f2dkqjww3gvg2ha6h9z07lfq8l39909wm36sech7ekc0d4nvdlpkwhy273vf8ckpsdngdqnhyxewdagmdy4rx5dtt4cy8687s4x6v6mlpaj9k2vqm0lycqs4x9n3tvmx565lgqqa2dle0zhlg2ma43mj5cwt5ql67ykvrwh8x2afygkystlkusz6z7slnf64ml23atx6dwq0szaqtv6lhq62mt78wqkprmp7dtxcurlh8skhk8qm9qzv4dd45nq70wk9n97yagdu0fx6zzsycx4ua4u8gkdl04qvje6pnnazntepaufvrx69eg9ll6a9", Network::Main).unwrap(), NativeCurrencyAmount::coins(400)),
            (ReceivingAddress::from_bech32m("nolgam1uma0ypg2yq5vdhe2fer2l748shunrjerjzxm27a8xkvmhcnh63n54vumd4p7fzj59qap05wa3nd6fxr6jqhxrur99lalj2edk78mld5n2ghx6tcvj2eaenh2hq7q35m7av6lk7lkl7adu24q24x2x2yga68uv8e08yxve8zmr6mdusdwy3jdf8x6k996jeg4sht84v2shh638r7t2933u2vqnmjw585rxlz8ftp0vqg78hls02xc49m25ldk3wdtl4a2yekg2dy0u08km57m03zta2r55st7uc6att3l5wnwa836fgc83gasjurdxcgkr4nan5rqsungfqmd72wznufdukqj55wkk7ea3gskxp4ca9wglaz2pvq2shu8pwfftv7rc9s5k29a2fhfy7dfkm8eqylkgn9ewj7ajrqk7a884amrd3frhk5d3z3dgctnlmqakl0anze5kjzf960ramnplr8trupz2hrcnxyff9kcrt75rnrgztklckzwjk86amjfacukzwh7s3dskwkjqe0gkhy3tgkjp9wzld4mzg8h9kd2a89rxvkft4ywwulu0gmkacnwu0e8xe7yk6qxkahu2nfdx2c5pkwttt26rv0raz63tg3xde9je9mhg20kpq02tngas54xy5j3qqrg5yszefmem2tpav7sz6t2agv089zgk4x0qpyc02w2407vg63ka65fl57x0hcn2lqjg48sttjtvqvgl56c92zwweyks23jzd5rlrn5pptshraeqx6dxkny6gkynwwavejsgndj7d8k5nkz2mgz5wpverapzt2ulseu3nr7ju5asv30w9u88rugc9ltte07xpr9nddawrckfu46teydxr96q8td02p7fm3qgpxe8p2djhphwucn0gydnt335zhy5svfzm9arzq6xhwmhgteu24qhr6rskhmlamk37vvsw2e6lrpzcgsch70h0qf4tnngpj3dkyk6we36xe0axrtnv87ryfnl58cx7yuhnex3m8dnmk0flr2k8pyrsfnsv9xcxg69tvdzrn599lz0v05vc49dc456daqld2w2dfncze562ekxtf093x9dg8clq2nc5u9vnvn26rp3yvtz0htgnv6v6rdmpacsw4786yr5pfr6hnzgasej3xldvzekpnn3ak2ncaa3fjthd04dewg4ks74k67c3tzvms0n2d273sch0wjjaah8eetjht50jyhxu6kvr3rwcppgpq9497ap8namur6lqu52vf8aet8qclqd3xydwyyne5z9qrufgczu5y8nk2ntt990gfq55ncd8rcatq4r0c6s9ma7lalcs2c0v2tuh55l2fgcp3mx9hqalc0w33pdf3du24s80c7tnhz26qn53g3vmewnz2vnnhc95yrmu54z2mstey3xjz00ulguf5plxncweq8qm0ta7kh5hwazpp7sr0wfc58ezenjpu2ags9gcqu9eu958qfx9wft3py9yweelx8gg4nseptjnzmwev5ewgxvqhxrwaypjwc0qxdrrvmsf33w8ufkezc828c9e4gmkxxw0tj6fkd4cnaln2mq2t89uqn9h0rdm9ekghph0kl2yyewdpw9hnnkqkvc7mxx67y83slm7hcp0ntjtyup7qdvy366jnkfrtksvgs5kx7lkjclsr6wpvcdqslapzh9muktajdy80rccru7y62txwsp4rdmx3205w2dwyhcpwjs5hnh0krdpquurdcgheq0eh85nnq0gserl8zcmcrt66l52tsnvegt6x0ptccnzkvyjfmhjaqxytvxtsphu85p3w0x93heeflmlfesdnkc5xsqws4j7pu2uekekxj4kj9kjavtzc540vfvvz9gyzxvfpjl2349gklagct2yrkffzcawq0cls0jklele4sdgly8lw3qmyt58sfhm7crugr0e3shen9356plyhkj39070mtzupesyec6jg9twlx778rz035f2rer6fddm5d0je79entkcvxmrhcytn2kfds3zar6mphrrcwmn435p2kakmkpe4nw2k5p5msnnsljtsetqudz8r5df04ucq6pf4q6ztlzc9fdydhgnd9eezjwc2n5c2vh2a58xx24gw5kuel95472q7jjx89m7e2tf9gt3xv5w9xllnelyrhgknckg4aump5qu22vnr67zupm9njcmd4ggqke6kt2m76xczg70a0dte0dhjtvclpasf8avvan6w2ylmp96hpt3wd8c7qpam6u03ztgsmr08k5y5qu6twx27wg0d20varxwa73ll2fnh7kl4tt2qhd6lzfvx5wdv7kwewk8wwvznhkk8k6hhm84f7quheqju7almmusgnpuz0demdpdep3a4vs3yghhu9ddj28ump9q02nmwz3phegwlzfq55qq5w0eh7ppvhv4laz0j3yhtj6ds6xvy6puar8a094vlkvknfwnma05ltt5l6l70srvw4mxnpvumz0pa7k0x6uydancw5xtuu7jnf4ecxx4fu07vp85t5qqcjpt6wf6sz77d3v4jm326ve4pdpp2zc95g0qxtm4tcc0hlspmsvx9h9syz5vw40yn7nrspjt5t00a7cw5ad6uan4sh9pvgcy998aeggujrtczzedhexd0kcfc346qeq7f7yg9z26u3nc6fq2g4zvqf7gkmtl3c8vr9vtd9hjpw7gahrwer8vq5u466zhv736hvy888s7sdgy6guhq8zv3jtewvd6glc2qlnlvu26ck7532zxhc7enkxpa33ztnrqatdgsjufsk89y5nf45xw9ms5e5wvzyrfapzehcfqyjarks97ma36pnl2csjsnv6g32ndz5txva05et0wrs8hpgcddnhntf6k2yf3gjyznucfv6sfmcuvzexdyh6e4f3vxs7pawxcqqcdeqm68jdwx9w2fqqqvk7e8vmxen25xz8sjhylwx2x8y3qtqp7q4l9xu6p79mtzlpgykn80y5e6z45qf72402rn0r0tmju7y9r0f947rjtd0cymp2jny5lse0csz04g5ph259wxcanumktxu6r3j5rxvkguux4z3v97gu5r5v68jrwal6r6g4zgxm4zdyp3h6v9a6hya9l42vsy0hdrmx9c8s70rkrjcuyen040q92dfytwagdfys8e2v3n9dtnzlhn8kf48jkngrdcszqzxdkcae66qdqadgcf8kt46ra59drlumj40knqfrpzus3yf0ef3k0lg8c6vptddymm6fq5fdnggae0l9ufk9p2j3snxjrfg99c8383wz2reg39rfys9h9peh4h8ed8f2c7e3vwqmdcc49u5wuzq4wejwnhcx7zdewxacqcc5lw7r9452zlh27c0g4yshggzzws8lvqepd9ftthv72j256ug7tjfez6rmf5jpz23pvmx9vk5vs0mcg8p6y9sx9y7qvrka90", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam1l8mxhzvdzld0hgva8lm22fj9t5k4ee5gmkcjrn7d65d2m96v8n0jp7mhu4af3av857rd7e8l6sxnzzng90kg6qqyq95v09d0ptzpa7fc40t3fdzx4wdk7u6q4xvy99vgsrd2hfmjcuk6ujns6vgqt9mjg0s66xg37q2q2yamme8c5rk6ttsxhnx9r4qkrlgftlgeuuzep9x6w462tdkmyy896nmse3ge4gl2uwj4c4zxlw4qna5delm8vvuntd0jq84tmkwvd7yuy9tp96hk7azwdvq7txhjtxywrlmdjdfgtyaut9rqsv58j2xqwce5awaehutnrkcxwgj5y6vw8cv8m5ma3jaq0rzkgcvju0me092wya33j8qh28mq3wd632zvy56s02z87lwdwjgmv4g05k5gvs8tpa6d4ft3j7sqfs3l3wxy7l2dk34m35eyrh9a4xlrkvs6t4r6ezuclel2r3suuqclnfykq8k7cg03nw2xrr5waz5fnqe2qe5kc3fnz48px6uvsr7krjeusqz78hh4n3473kp2jy0qkma8h63rrqx54u295g597t3jws7prlljahcp9vqvge7mhm0jm5rs0dyf0cnwxklzehj6eh9ky963pp3gvuka7ppk33z9zuwgjquf7zhpvn3t6fv9yae060crckgg3rp0ghkuzysygg9dqve3qfhnjfzu4gmy8p0v2wcdqzdf5w4vw8nhm4uhz5lh6vxpuldxgws92c5cp75pfu348svxe543l8lmksw5a8kwy09l9txqprjt7zsv4x8y4288x7mqgl42vea8kksalykxzvf2qjmn64zn9d8c964tvycwwxu4s6mqarg4jmcqgr58yf842xhhmc2fg6mv6yxdrxg5d5j65uw7zqcq3wdz9r3gu62x9vtjhel5y59pms7ddcg9pdzsr6ujjvr4ruraw57huq56re2tydakn6suy7ldxxlnvy9qz8n7jxkd8ggrq9ls8hce566467dle0lr3mw3e0hxwetvsntlpk240wx9ndqyacd0pv5pkvq0azpsq5x9mrh2zp0gydgmzepfw87acrpq0nwhyy9mfzhx02cfah426lsj88wsv5wydcxmjp7y633qllwm6r46s9hf8r83e2w3nu2lqf2wwyw7hpm5jnzwsdswmchknxukvc0hqshkeyly5yzeh9qu3g4numg67532nf3pjku8e8dsq6x4ccmyxydlxz68r5m63whxf68uj3mpmd4xrvhvggjsd57mr6gxpaf6qnztwf6g3r32489502ct2e3jmaaqc82przhqsgsphelws3swp5clw6mn3cgkn2na7r5axc4mu33t3nwy6tj63hzgx7cs5ak0gy573aks8dp9kw6vfz8qgp5sv7mxa4ujwkthmgj6shvlp7h77www89lrn74gredhe02fpsnx5stxww2m2d0t0e2zp6am0z26kf7ev55tca45t7r6lgwj8g4f5h0tk5sg25dagkha7n28p42km26zueaa7qukn4ts0q2xxqew02w8gntpu3p04vyn6gvqu3rtxne439yz27uzd8zfpslzp26637ukk46xhg9esa2amm9m969n00lt3dpwl476ap46dxazfgq402yxdmuv6cdphhg0lclyqm8680tzfzh368yzdwx8paj6qneta5yqjhnry5jd2lujfsjx0l4vdpl5tdfmmcu0qjtk9yl65y5jx736lk6dlygylnya044604feva5pfxmycvrj29gqlery7hgttdpthvlt588ujyr8jltnlmzew8yxmty2n8z5elmk36cutj5khj5ec024vrg72mef62dngj48qxmpmj5ak6hch0e30r722c50uaeyawk2sjf77au69uhuk9ds6kgpayqmeal7y3uhdxrynfwc5zrhvn0av9u3dzvzs39lkf83tmhq0j9d2krpfc6msxqkjadj9e75vj0tyx8xqty6kdhp406jl3jl4ezmaw7v6jnkd50hu25dcyca0t7937yp992gn4kpjf02xhj20cy4ue97pm05uktcu8szznjpg7h6hvh5arw80de2f2yndx3qk6ltyjahjkt54vh6ygrlw6z084arxdhnctk4re064fqllhl9y0lgfxgamzkcuh79389m96h265qqk4q4nsnkw07fyhv3wqm83m539tkgg0z5etd4rg43rvhsf57rrfqjmxngfkjw633paq9xawfs9re7gqddn6mcwhfndxnrum229d232pmea0v4a3ua539cu74xsx2nv8733wm04r0ycejudkn76ag7gqg2wplghgefuvgxejqxyy6kpgxxxew9p5memff4s5f4tsmdw8j5e0ywfnja6lkplz9z29gelpxf5eyl6krqju004fal6gkkglcquk6vgxxmhcs4msguqncmgeldyfkr483ks2crurry8kez22p9ht4xfkl0z7klfwq6rkum49tel4jqt835z39tu05xs0304le0u6k2hly9vrlme9tymk3g2epja09j2rydnzexrr3wq6svf66wyd5h7ewyfvf9hw7k6l8u3sd55fnyule2jc76luys2rknkcux3d9xad65kh73rzlg87n8r3n7cs0z750q5ujvma9p0n3xpcc6mvqgh2dkuyd98m6n3er69dp74tkf5lnz67wk5amz0awalw328j4dq5cqjcqr8y8j9z9fsdzsq04wfy6j20erlyxxueup8zz5hq3fxn2lpcls0vhdkjv9zevnp9rndpryt8hx977euj33peh96344k2dqvp0dewyyp93lqrp5cq3887dk7nndxg0uz922f9k8s2pftanswf6f0uvttl3r0qy3fqwgfrqaxep5ve2p83ncxtpa3h3f95gpqy2hnu65p7ayen6mvx9jmxcf09d2fqvs8j8czvy5fmhur5v5y44u5xw7w4t9vfhznralfs09sy5rsj46t2vks5l7ku0u8mevqfgryf27zv5cwa4kuaseheh8qazu85jtkyv9r6th9hjeu66azmpc2xsufpspqtf2yjnr232efhl5av6y2fg66hm8d4xc2yxm4rpes2mzmrvwfuc5akymxh7pw7mvdlqrle3tgev8y8m600s4zcsy4793tnch0xwtd8q6s7qdtsdnq3tt3za8tm3cflxuftkkury7amyugqg9chvh2shxf9xr6ss7wk5q4tg3q3tfd7je56asjj3hlm8zzuhspk2432wx5j6t0y62wmt98q8ndz6ntpcfzyaplhu88wfa7cmf0edks623rt0hykjm7yt43ce5hv0qxwlvdkz3xccmewfzyh3kty480japgf2u7heyfrv4u3vh48s6xyhygx434kedjq7qnm0w33hkmkrh680p0fwx6hcva208a5j0fuy7hnskqnwd6suxkg9c2y8g5k7s7t20gn33k53", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam15kv5rfkp4y05fqgfk3qhkjrp3p435e56xnyt949lxk9shj9h4lj2urg70nhk06q3frm0aep2ar78rctrapdy6tp66gqfadh3jzvs0walf64m5tcwptmtyxgk899fl00qm5n55wj47g4pz5x86urk4gu9z85qdrwu53tysm98sexxv2wgpvse0437ywcvcxlulvgk9f3ww2353sjczjm045j8k654znec2fkxtnlhw0syphkdtkncsj2n5g3dfwlqcnl96hz36nrx5937qjfjuday9uqemdsszp73q7f9acrawdcj9hv5unfxuz40mk6cnw8w0a2eca8c7f6d3yn6h267sctsteqczue4gwfl3sey6ztt2vxa85sth0nrawdytft7ec06frlykq5y373uvqc49thj3q6l69a5enyezq2q5dymjte40n9rhzklxlzy0zar79a60numkqmxdydjq44kwj6rukkzanpeagtu2l8eukx6vwzyals5an65e5ee7qm6j382c0m52ldrwv92anvqf7pa049hcx5zqpjez0j6t5y2mxk4ruu23mv8qq6lp3syxqck4e4j7dx423kkespxph6zsq8dkh7zrapexmaxluhegm2fup6kswk5sqy8jwt7upk6l9ax9a7cdmvgxdny6c2luyjrcfvyelyugnqrrtdyjvrlqn8lcpqp6jlal4l3gel7ydu5qa67hzvu9zwrc57sqv2aht9fyypx7u39m9j8yjapufxxg8pt6zsyqcff9pvfkxj4xk4kpauc2j394q4lgkgu30cplevfua0v6xg0zdruwrlqfqzttp4msjxl2y4wmcwt3w906yyrg2xa6ypwesfm85mfkem3jgscw24fh4nq57ukp2um8pv78tvznadg2p3wk7r4ya9yhdcejhjvc3lr6hj0us2v5skfqeguf5u8u8ryru39s92jrwqgkfjyk7hmsnrcs9dvyw8acahc3ryff7hfxvmtwp8eu7jm78nw48k43vhp0ryd0djwfzwxpy6jt6mg83lvmpfgnlh4xp76mfaxgrkz2j88t5rk59xzvfys3a5lqtyjzruk042g7juptmug3vqdsjuc907rctw5nf4y68dhvkupfn4rsfzu98jcqlgv7m973gf65wqlrr8ll27g5nlv4cvuz0fwkyjmyku2tfwe8m5k9ynxpnqmrx035eymu4ms7kkl3v5zfctm9sp9wflxuj6kxyeaypqmxgzqhx08lfkjy4r9sl67m6h76rytmcsall4yuwy78zuhjaqgpmupvlcadqu6c7u0xxhyuj4fxj7chzkgr6ncgj6acustfv2qjwrgy0zj7pw94p675c6e65rmr5fkljlyt97nqf5p25eueapy33thhcap5tlp3cmvm89yzpvjqrhqxz6qdd0k3ld4j4cugk7vjr0ccacl0cldegtsa436c9u7at60uq5u9n6nggx8kmjn79u4w72qzgxwxdnuaejnvvxftj0vjc8mrpvz52ksw4myl0jvypvs8e6t35wp0vd4eq48gv2ezat3t0hsr07849vlmqtuc8fj47ju0j57vq4yjcvg4d08hqvrgg8lnu77rsvlnkm7persheyyv60lnuwg52ca8kv0n5sd2h9evswz28neu8qzxeazsxl944vp69uruygdrtyxa7m80a5qmja22lhhnkqguv9fkfzmnf080w00d6k0jujyd7wxq7tmvevrksj2gjxu8t8ku9q70zyk3mkcx8px5tyfkewvklpce5mnc0unzcg3uvklx0ymx0wwfnnpd09ywmeu3rcyajt7xkmxsza5ayysafpxzgq7twff4k2ld8vecpxcgc66yjg9xgcuhnetmzgq9lhasracs5x8wwctvw6h2zj2l2ujjmhmk5mq9rf5r03ql56ywuv0xfgjlelc723ydurrdmpr732gr2j6fvc0frlv796evfym4ufuzh8z2jg67pehvznn6upvay7kcm2srdhtyj2vdhagfequ7qyag63pszd5sx4qv67nx9mn3en8xevhfty9ygy8shfcj2munehppzv69ht6096ryaghec7a7wuml4fn3mt3yrm0pkr89dxru85wlz37werf9y86mthy4nfe478mk70dhnsha82ypl25g8gy480xj2qdmaxpjmwcetd6c6v59v4na34y5n34lmnt6aay264e4ntqra446n498kzfuh2janep090am0x4enjx3q28el8zflzcz2h5dzvlkp6fhpwjesg5sassvdpccjae9mj3gr2lflkkmc2xgh6zde067hpsku9v6ly7qmcf8hurh408uqnnz8vdplkv8k3650kgpmpd284y6shaxkvfqwehl69zwjdak5vw40kqktjgk2ceavwlu787wezhzatu0u77724thz8zh04xtycyc2u8nq9d9xe5qlae50v9zumyswjuaawc4vcx2zvg2n240kyjxky8hsd04p3l6hjdl7hvxkpt0fzfke86x9c9g6g8e6fht3eq8t6rlel8kdw3mfxcdudkkk4uxjyp3afqg9uf8n0cssxclfntn06xejwrwegcdngd00f4v0hzm0cekrrvm2geqn204upwv6g92u0aq9m6988dxenaxml233n4tgyjej87afrgcajz0anpaa0srcdku7lef59cu2pkzcp28z5aerte9gpf5qhfa0lu0wft26hxa5n7f2wgquhpxewcvnnzm0am9ftvu5zssplc0zgyv8drtnysftd2v7402fjmv3jsjstszrnu3gvlhchfe3qcjjuvd7gfpyqd8y5r8rwpv47sfwqy30ztuy9hpk6zvqyg663d0p4uh3rd6kga0k49mevyrn48ufauqd45a5ywppzgj75tgyuhqw2a4tn8dgjlgkt9etj66eshsp79df924tp6cmff2ddn726hdm2xn7p57uuareujfuhlvmp2tdhssluty6smsydvt27uydvutyrcn96kma0r7hdyshd6msa9wj4y6haukjygu6zd07y3f03q6w0r9axa8kc3lqjzywqp9nvmaellm3mnmsgghyurk5llzj3zz5y2jtcys78zkyjt9d525yjg9yxyg2l8s64e2afr5nzxpyrwq7hm36ygnjy4mjdp58z5u6p8ugh8mvzgwcnrafn4msvlz8qcc3t83c5ywvs7gfdrl4kdwacgaukszgj2e72zmuzckseyn2lrzgzqgqchchkmvke9hnyegsmgnvlhjume8f5mr8hcw7yy43yktp7pvn878nstmaa75lfqdnayanhzmz6jf5r0mr0u3h0nqqk03f94h0kf973tlnw0ufcptvp25cdpy2ea862x49aez2s9s9m0v084ksraekc0xnyeaw2hmchl2l76tl2ztnr2whkjcjmr3tnhm356k0aq2m88z2y8lcew96yzjp6edcu", Network::Main).unwrap(), NativeCurrencyAmount::coins(150)),
            (ReceivingAddress::from_bech32m("nolgam18h2npfwma5h4medmyk5f45wpzw0tyjnlvyvspma6vatpdk3zmzydrvuh6ysd5c5677q2qq4ws9nsxa9p8h4glxyf87wpu5jyjgfwfm72835txewlrqggamwfcf65dm0uwcrz95dnh53lrmfa8ghh05ekaa2cktg3jc3ha0gz72j09mqu3me80at7l4vjcy0eqw0gczrhqdqm7h4tgeg63y0pcuhw4t2k42te85h83lyqu0v8khatr20qqmhwemx5ke3cv8u6z8j5aed089z4vfurucjym2ss3wt85jzk7wse4tza4fagweeyyrqrn5n876he9qgkafjwj24fp5ye6k5f6kapnsa2eux3vsd5t3gydgq67s7kst9yhh8yycw0gq5gv82tdjeyz6y4ts2dka6efk3v9yafcgphuahg580hda9z6me24v6qw4ly9492dtqw93lfwtyllzg7kudwyr3ky2vp3r8mhygw7stvqyhnnawykkm02czdjye3vy06mpzjvfelkr47fhdj3vdzuvqez8w4z0sa7vjw027lypzh4f8xm6ytl7hkvth7jpmkmpgnm0pysrts5zdkhzc45qe9lpx7pl4g77sr6rc6vjhmtu2ylx0n38rul35dns9g98wv6gdfgx7scp8ackmw46p2qpe7jyzmdwna2u4j0lwe456zv4j30qcvt09wtz62hw5zsdy3jcda6d8vemuxumlukxdadv9st5jqjh49kfrtgzk442j3cu4vpew7kps6pp4008ntv944ahqlzps3yu4xe0wx2nd5wpeyr7tkjz32qrfgs5p8q9865a45n2266jvwcu6awj6gh4wncefrr6wlatk3xg0dxrvnfmr4eh22nuj3q9wcvgqu074uckvgxhcksl06tm8rw5gxve05f39g0a052yalju67nh3x80rfrgx6pqk0szh6l4mngr4cerj39z4t84wj5dw0gghmvksq8fezsdsjmez9g62krlgxqe9kmjct3d5wgwy24hmwhupfvvnu28x3sn9jn3x29rlmwgukgtw5qvygd6qsdfwlz3zxdd6flt7ycwf84p8nthsv558q8ze7qnpa8m4c29f26xy5mhtdlyu5smxa5qenjzsttsmxw72098kx8vpv538gwgtwvx27f3ezy6s7r4vwetg939srj0ax3u3rhmdqn7p8gzxa7jm4vwv52ntqadnjx0dw6ep6v4a5qyhzudd3auh4ny3ank6nfatfy3dur8646z7vs2dyn4a3stafu384r5krjcvwm7qalfve7zhca4tk24m62ukdxuvdpydx7sqpfdyaxzsehexpu742kruktu49rkrrtpmk4frmrmu4qsc2qnvp8hdx0tshfth5ns7a3t4m5axsr0dwlje6fsev8alcrxg27sjgp97url3ht9fs8xzlql5833mjnu5zvuah4k53llplm3g24gzze566cgex8snmzqt99srdw86tpqrzpynf5r0v8dync8qaz08ddl8xh3ajpg36wtfvcwj2ghd252ww02hef6zen2yhhef7xmcqsl4nrludncxygl3j8yj9s4cfzgs2yvm4mtxg3740a3hyhs9fpd67jpuwtnxzte6egsx5h4skpcekyecjmmya6e7eqnay58jjle8ymsuhr78gfqrk9lfgdnnaam3ft4nha8ltly2yq72e6njqtfsa2074pwhew2venwsz7pgyya24xzr7jqr5ppjq3cchdnt8anxnltjum35hjs0amvfuyygjcsjutzqyltfn9prtex2lwhhqwsv7wptt93y8n9lcka75fg9s395qfg57w4dawzue9xx72c68eyjrh5uru3h4w8yfez2m0uphsp6xtlacqtqnntq52h4a8eejel9z36eekujphc834qhkmujr4p0909mzd6fycx27ws5j37chqngmduqfeh8pcfsfchrzul9es7kxmmy5rqespnwaq8z5z9rzvu3p0tpcsv6rfly79ue2pgu3zxn4mrx4maec0zv6jedejp6e3wg84fknle67epwcjlnvcp6g6f0lkrmvylja3auuwytf3xm6edhuezs9sauzckfse4cfg7d5yh89hn3gpntgza2rv2ffsja0gaqca4vhy0e2ua8mp3rqr2f7wxh2a0554dkfmeggk835g0539mw5268jj04zvrs7k9u6rke6mh6mwku6r9tfud2ykh0zqxrvx536wl2duhsjf2jnrwt7pc5uhz3thcd3gx2ytgmehvshkquhskfw479ukke5w2kmgks80fc9knqj48e9hpar4p2g03ehdepn35kyq3ftfun4m70ppgpsmjqfnxs3hs5ly7lg3p6gcsu77lmc6p9p9ug0f73wgrn878476kwkwqntx8g9hgngl5nwsmu95q2t9dqvfarqczztyw8cgehejyhemlgxs4tfn9l9qmmm7dcqlugv5t5fzc97dyqgg26p6fwkqygqllmlsyvnnm7cjz0cl5ap3yd5fknr3vvzyhx5ztverr0um4uwc4fc9az3n03e40t32xeec0upny0hnuszt7t420a2466hywwwlyqr9e5cka074szyuj52vwpsuvm59qf3u72pw2kh228ppt3f299gldzmf3w2lnqgfdxpn56fv8reqpqd8qcl4tyltpjkaa9l9n942vc5shp8del32ac5a52q403m0apw8ye9lvl5sgdfdkm2644l327pg32d8m0pj3phlm7mysvkuft944g4rwrwvlqemgsn0gxpdewjfxjkalf7ce9lyavr558e62kq740hy448dn39t5dytcr9n20njshg3f634axlfhemf0xqrt3q49ngf3l2gg64qll5m7y0jam0lahn2dkn4zg3cv8g3v980kejmwvwtm658zkhqfpdnx6rxpc370dfxnknfv0n67gl93hvx7fymg87sqccr6xg28c6hr3a4j6fp23pym84tzvce5emgxk5vhvsy0g6n25jzgkhrptmdsd2l0p0wwyau4p000x9x5sctl4m705vtpa38fw3rjshge6ynrh07ldkvpenmrvn65wz6apxeqj66h35tjura3pjrjpfkft72lpc8vhfzfneu5qnz37kf4yysps8qpzsmhqms827s3qv7a4fp69puwr70fa2ue3dxj4ssgd4f2c66aa6t3q88m6thywva8mcj0438jl4rmz6jhgh75g3nw7fsxhmnf9whg0hp9mc0cpayvslerel64v5tuhkl757dwmdt7azjtxduhn5cp8s24y2tghyvwlflamx8xrv8khyce0hpg4trjlay6e0jzh0gpaauarrknd4rj7cmux5ju290542f7kqly54xukdvlkt66yymnx8k40fefeqtrs4l59plalgu7xmsj7hh98r3z40uz07meelv438zqn2rydfrsu4tr7xjy3duzp", Network::Main).unwrap(), NativeCurrencyAmount::coins(2195)),
            (ReceivingAddress::from_bech32m("nolgam145r4mtgg6fsz4aakpgtccc52ghpnq5jvsfx825t5szwu28hvj5nwgweyvx42mg5hvcgwzrg432fwv62sanjfzgjuwu2lsammxd6smz98tw4xmln5a734z3y49t4h2ncl8dr9t5ykk9kkcr5emjhxte6nxyffm6xf0pqf7l4xygv6vm3x66punvwqd3k6sxlv3y0gvumlsw5a9qhfuve3fls8mp2464hz7gm7ccljst9utk2pmzwpmc52p4yzvptx5ksep8echmj6k7h4ydwqm2pjf3f5me2ugy9937sjn3s5h66td45rhc8qhvtr3mg993g9gyl8t2txcwjc9hskmyr4fd2nqfknrze0m4ky7q988fdvyh6w438h8czwh8wwzm9ftcz6g2xxvm744jq4pwk83r0qtvcvyqw3yv6epk70mgjgjjpxazq4t8rrncztu43eaexp3934uhnal3wd20v9dqc4amu5tr2vjk88fyhz7j87w6rdqs6gytz9le63dw7qmk2x9xtllme2pzrhvu22eka7ey5j6psrrcxn0udfs8unm7awnk36drvvz23pl75rn23s4kz9c60huqs4y9ldqerqlrfmuvvypv7jq0xhhrva5vjn3dyqkf4t2mfaj87gnc8azwnwrldz54wgktq9d9dra6slv0gf4hxsqaf6lt8ffev4w9j5sycppkhtc8xm3kl60hjejjevsdy3klllhptx7twldffern79gv3s6zlu7ylfymxhjnucnhttl2ft9tht2n34slnef8mv567xur8lajatsfs2p5gafh8u40am42atw0uk9twpgh32rsx5rsu9t58m7eyapx2f7ujn38wrmj8pux3h7ajyduvm7rwhguduww9xfzqv4dlfd2py3ts7mpqrqtmasqmfr99urrn6r2wmgdncywhdklhdvuvzwk43cr2zejux6tsged3stpnkkydnzy3czchcggsz5jytz30859m0pf5426gpfdpnes0wwgp7h5tjtj769zrwunmunr4geh58qazpacaj8euvcvt545vtpgd9rtnjwfj4jgkw2fnym7fpwq92xyqcc80423rj2rnhmzuvmepqz3lh23twt2r2q0rat5ccg5kwdtguw6n3fcnk4576u4esyamtsvvwnx38jexwz06ntalngqhk9nqxytqy9e7hfszk6najc9w3g8lkurl5ttc8l29pk26j55h34w3hlrv6na64fakt3hfv0egxr42hehrnzrs67mppfmx6cffufucwzva7tvr84xw25gmr2ash2eum3cnc6308nr3f0wrutvu76twqvf9d4z254t7j5aztrc0z9va9vd0vj0ar4tfpptmqwuwhvjcsrgk0hdgg8us2du7nd82anqjx9w3vt04rf4azx8hdrr5ld46lusq3laa29n92x32jshrl796zeep7warqncuzwqg6gr2txa3ljfegcgyz5etxse5fejgef2snryjstrwttu6n64f9ur4y8dwxm2qrr6tl6pg63g4tdp4khumrugnnjg8mgj5auzwnt06q8va6y0audujxdh3qyntx2xjcfh7xk3qemd94r50rrcu9s75s8sd55yp4z66ac26q22yy85nzyneeyk7a6u4qjh9flqp2vl9gdkstmnr9g447cfkfktflq3an0f6sfncqc5m0jt44dml3cgjr2vjcq77vctqguypxgyyjl9xyahcwmlz50c2z49jmmej0fu8qet5fn0knq7l8grq4r8lkavckwyyl2p7afpwewu7czgvncxt2ns9jkt8mwv2sstuqndxkyagtgj7rl3r9m9dxrtqjjxm64lw9mj8djxnh52w0qqe2xngn8ls2gshghl4w78h0j6dct854y2ecq6xzhauv2mjv6anx7sgjjgh5ztupsqx4hfazpqp9qjfmdgvzjfz4nhv7a6wmlzugq8j6xmuyh2a6el7ld5qnjxm4e3y5cr4xkr5z7nrl0gggy34za5h0x8rr3lssdjrlue2cx6clvnf8esv53gmwkhjhkp5xpnmk0dna34n3z8nlw0lr7r3f28aradf02hz4fp5vrxwccf8npwe9m5f5rle8uva8grqshjcey7hwaqt0pyjesh3hgc82dhaev9ulcjjqrmkdd84gk8wcvyjcmwe9y2mwwrw4ct7uleq0kt9sfycek8dg05z8mc6jacvlj6wckfj5xf90jn4kyhedj6fu66he9v6zcl4lxnfv92dncvunupv8f7rjxccckjhr3mczvjdpfqn0mmjnzjn4gmv7yue2rz5uy874mge6pf4pj6gfz030cc3447cfcj2fdhtjeqjj0hnwhaz6jd8rxf2v252qsxtys2jgsfnulss874txnwmqc93f8shegcrtsnke7l8vwxmmtr43srnnldsxxcq85mrr95qly9ex3el38rwwcqrxttpyf2x0xznyr62vpkwmq6pxvq0p9p30scgv0v96fuppdr0n0snde6ktfrxrm9lxlf8y45kpefmp9r4zmtys9n5euh2ejq6wacg00ln6a48e4hy0yc46dapwk39x6utky8jv9ggre0ny22wsrdlr65czmqu5zvmqual90asv25f9d90gkjd472vvh9h4pfejpqq8xjnj88yw0fvs42upyc2amr89helrkw46hd0mvk3p0jd8t7c5gup8w3dpfyl43qaukfjta9w3mdcstw4s6d0ca4auvswcmfzdr46d9g2akgpm3h263fnh9lfpvl5q4netvg064r52pwcefxjz0r4ahkd5y4zpv4e4sp90rrh4hw8htk0qu72gcu0fcad3plkqasu65a0z3am0j25t6cufef6r533rze9am053s2dpgkqyar0h094r56hwx95qw7qlx3n4kzgqzlgpryfh643qu0n76yhfm5ez6vx3h79hjh4p7tttqqdft36jh65e5hf9pqps87splsv3pxdvft3r6l3jf5ej9wagep2qyf6eyurmlhh99hfdw0lxll9e8mwytquvfhy765jrex4dcwmsh3lr2a6wfjnjz0c9f9rakj7shgqehwg46tff9dqnucey9mcmy5jk3mq7cm42wlsqvjjknft78nyhc2qq02f8nwgs53q4ks8zquavkrzcz834wjjzzeqm3znwxm4jhfg5kzsrq7rjyepfvrnv24nxvm2vz3alk3rh88zpwgurex9y2jnrnn6k2a2mlcknjmvls597qetjzfpaetj0y5g4mdvy5ke0t7t6hfc06glx8982mmpkadwtamtfuzaethrzglx8j7d47rz6mz0fgq4z6vyjkt57eq3syfhy0spt5l95srekv0mpcn9d5x8v7kqlurzl0etsj8pe4hpz99g9nfmhukd886f9q0aaxvg2646mauzl6f5jduh5mfjzk2q7m4na36knlda0hptjjp3wz28", Network::Main).unwrap(), NativeCurrencyAmount::coins(97)),
            (ReceivingAddress::from_bech32m("nolgam168rjaf0mm92yhdg26s9ch24a6tl0yklkpczq5hemvwjeg0gkukq3yghmestftg6l7q6hfr79zmp38rt3ccukl0395udpkla4mceqw0l9r5mfcw8qljg8jw9w68qmmrqwgxkvaf9gg3rka8e5sp3j6c9fgjw6kx8aagaw39kgaqaadzzrltv9jv56pt9j89hh3w85f7el680ysnvz68nknmdhjam50ng48w2dc2jzx2kqnsrffgk7u8v784nhzvsu7zv6aem0z9hqmy5g76ya9lsq34x2k6lnmvlahw99rxvm05k935qm7vu78z5as07fpmu8ey47ccnu632ugw8cffgpnnylpln30jfg04f63n5wc8lz58y2utj0nmvkhyfg7mnvpaq746wjnz7zmfrt8w2kea4dk246euu3ml4hds3773kywwfx0z07mjthvhql7cf8v5zltf75ae929xvhsj7heea5gk48jjfkycg0ghm5277peed3d86aarv83gq0uchlcp3npq8q64337h6gc6eukkawvh0qucft4gsagtfdlv0p2qrg2j493382a2wmr3j3vypg03jahjwrxjs2surjw2ctmdh2nk4c06ltusprh0c5e06y99q3cr7hqmdm4qwym6zs8fh0ujrlzqttsl7eqkggf2fl7cuxln72dp88n9r78ahdl60ar2jvse0rpu2swpr0zkq5ntnlqev9yv8xqx98ey0l82szpprfx4wjl73rj3x2lpg2v9qzm37nw4ghmszhj940x8v5r9jw55l03lmx26j5tsfy2chjx72lat222t564w4yds59ny8jx7j55948mfx74myq3l86eanqs0qeut6743ygapgz4xtx39927cnr6ksapcrhxc4xute7avfxau6dl0e3kuja0grxl6p9r244cwghzfmk3fm3qfscfx2lfz0c6qsuw9emvl3px42rmch9tde5fndu4p6k9qac2pp9uzqtssk465sew4wtu34m5yc77tg6qjw832j5vkd9dqzdghnunpu7txv2hndv5xj3pu2qpdt4y07pcfauytkyu5jvynucelzvjghee9vueudvh9ch4cvve7hxq82dq0rem5m58wauu5lfuund0dj4c467rcsq2c8nkz8e5th6u97detrd840305vakz7nf6txwrjl768p3f369qx3yyg2qrj5ztm9jrrncur6tw054tm5au9kde9njg3nd9klelajnnxyep7kd8shltc0sh82jjlcwjvx5pmf6mhjywx9vstgfvt57wsgz5g206jju3dxjvztk3qa0zgtcy9h0r04v8dejaw96kycnqjgmws4d609er0fmx50eyxwdfru0tn439wac2e9q607lfhwpgnpje3kqefzknam8vxj3k9pcegcxat7c8x3a60wr42puc88ugv226t4cnqwmg2ld9mup37wgr5ms33sdzvdrytx4s7cztt7fw7l02fvun24jdx3azvnwws7m2cx988gs54uyadxmwuzjkgrd7de9vms2ulscaw3kxsd6879eesfjkapz6n3vty0ale6suqwg5aqzwmyh8ztt6yy28wwjq6y4dhhawcvswppupy6202mxghswtd2azpwzk7gxfdhlc7yfe5el0mlxp2aa00z3jety5y9mfwy2grd0tze65zq0e6znrjnfh9glxpxuq7ve0j3skjpn9c4tkv3dth48vtlldsl5a6rgvvl82zwl98nd2sr8qxl40j5s57mn05mt95fmafyjgtfd6mdykxyj7ka3a9l37vz49wp7udwjnu9udztsghp6x2cevncw9k0uspmn6239ah8vyrq35kw9v677h0w0s4yls8ltelykh532td67npaqr7ax4r0hyj29wfmyf5q98xuadqyv5fth22vpxx9f6p0utz5c6lqjzc8kny234g8fqrtfgde2vt3uw4w9ecwtty6rx4m9asgdhzhm2vvy0a52ekc926g9955ksfmtwrmqal5jnnc03rlk4pm4e37rer8l4tdzj8ned8j7x2mgg9zkdls4fd8q3f4ce7zd7nu0acvad7ptu028d5vyqpn95q8zeu8xptfgt5mkya0xsztefe02ekuc974slxfn0zkpxc9juc08506utvtc9s8l77wud8fgjygjdpfjlfgqtnlv6z83thfjhc0w28ngec5m0tw7a64nasnag8luf4uljc79fj92mk4au90yps4hk78gc743280psc3f077etavgnagzhw6pjumzcuryz8xkxlguw0vlylq22e2j6py8c56yaas4qstnxasuypfcw28lgwsge4y4zz7u793edyr6rkxzxuscr2j5z4qk56dy329e7q77zrqga5f9elpy0ldap6xsmyzhtagmhcvkar7e7wmfgjne0duf0wxs299qw8kaljpy25afm5sdfadk5nykr3jpnxkkpsvwsdlhughraj3rwd7hckyzvpp2wl5gmrluhsn4ac29kak3y7acxcm9362um2lau7vntp48e4lz8r8najtsutyje9j5ywppy0vlrt35at8cl6hwcdg00k74u7k3yjh55uqugtlewgg9as8hdmkcyuzcmrdllcpe9xej6alsj47gc2cyaffdyzaxg8e3f8tkg9fekutau378z5604u3qh70l5xqp3nku0q2xtzhzu7hq60a6cvvla2zkzxdnucugnxarz7vvh4cmvha6t7p4s4f4se27p97d86n4wj6qvsx3543tvdzvf8vzrc9xwt2e6ma972r4xm389pn0ttpasw7u2nu8k5az0l996ly4dejzp4yw28qgzgxw6lm8hycvnydjhu8z5vemagtqf30g6syht0p5pn8ttus5mqp7rj85kqxggqc426gt5lmd83yzyxpn5l3p5wcp4dsmjnsjzr4t2jmj5hn6aa2hnwqmh7rtuhzkmdksp5rpv2nxk0sr2f4ttjtph7rwechjekqqmju8ekvsjr2l3uq904a6ymvpch6wqr2e4538z2l6pjf0cxkv2sv7tq7m8lukpl50w5h0zruhg7w342wwfwpjqp89gzng2805yxgdmfnsde7myly0wjpm76sdd5lln8sl6gt6nljrxq024h3ufwkqpmgetde4ed6756tkqfdd83ksu5n7u30yyn43lqc2fjeyne3yxezpj4rtzh2vze2znf43qylz4l4jr73uv9y0khem27k4qhgkftap9fqk4nk9h5c2n5fxs3jdeyj40nx20acjr2k0jngesx5d8ugqw3w4755m9vy6ufsnuuzamhkncsg7uegd2g5044zppxr6t9220n5wzl7huqj2nm5w5lqfuxnud976x6qt6n0nphwlug08ph6ejcy7qlqxu90u2xt4alhlmhy375z9myhy79uu876cyel5kf7j5c74rwtawgzhprgvq52z5fyprf", Network::Main).unwrap(), NativeCurrencyAmount::coins(485)),
            (ReceivingAddress::from_bech32m("nolgam14mtmy4yn9aee39pazg02r29mwvxegh7e27s6fjxvc4tvyddsj4ugdkcx2l7jzqnk4pt7rdcw9u8eccd4unqkltauaaud6wyrxyqfge3xp3mqnqx2qu83zmfyzr86kcvvzfs53rdz8hpv23r9785425ucqtamfa4f442wk4jm8yxxntgf47xah7xeyc3xtm6r6rwcjsr73xna5a8cqm7xlxmwu873cxc9wenqnpzvc99x57amx6c79xmr3x7jne6p7w8sqkmueayp44zc2m7qz9n7llenrejymwrnqv5z5n2kjf62r392jxlvc0y4qtspwh8mjwrerr53az2gdlaswsfkwd7ag4skzhltxv06yhqulr2q0vu2ph4gdyr59ldfqlc8w2px8kvuc7666xg3zpt7haxvtpqygffwwrvv65ack36hwsgngw05z6xf0ltuj87e94u7mhp24pf37zv8fuzjmuapkfl03cpq8vnaj94hqaw28lralcp7464hw6lktryqqwr4jw4tcm4vv9ehskxdznf23dhmfkdqqgr5pajj95kcvxxhefgwpljqksxgjvggagcgenj9sn0jar7tfsfpkcg0zun3zteuh34a563yjvkv6c2fzqfy9460kcw70mgz0evzked4xgjeyvgt9zwlvlwx5uy7rghwa5pd5x07zsyxd7y4phyxkmty8gt3lmht26aygvjfk5vl8f4l9x8hwz9j3d7j637u0mqzmncc8rr5294ds86hhxgpypellstg2679rvmctthcawv2k4g2hdeqp4hj5l75kj4akwshraup2zl7l8m0heem7fgsujzgn9803xwmshju7r8e6wje5gs3lwp6lpwqrwkjzvs4jy0qrg98ckr6ew6n4m8x2t4g3vs05n5uucd5vefakcwlwrcy7chy3klk64dmvnzzygqp9cvt5r4geqq68gll3sw2a3kdt3c7gkq7pz723jp5ejlmlvpqzj06n9x0uap2vrn7a93j25p0adhr9u0qquxqf7s6pveu7p9yaed04z5myjmzfxrpd7cgpzyvrx0lqvm3w6yf7x8m4zejl9smym70shn7w8ldc96nzrq8kejudjlheyappya67k4ggw8zpeaqe4kkkrkkeq8pqt4uzxk06du4ndsyvzaejcdjt9g9wk04mkjwut0eg92f8nf938wu9fdur289c92vlmd970qfztyn9a2wgx080t42j2d5627qx505g2wujq4rrs3tp8687htlgyxfe9ypl2v4gvqmjdvvr6s67shk4tugpekmrtaerqd54waxlelek7trzx7xrlj6knfpk30mgeydgcdmlvzknmp6zcrhj8uwfdullfm9tye5zd2qdwq3q0m9pclcfl987cv6g5vfd087vrezx3pgvmsxwlf7s2ejrkyl7uh7kdg596nyc6uaxzm6adakwrxt2eelru0d49nzscznshhsyr8gk6jcz6904dr0lfkyf7p6kufth37a8zlt9f00re7qxqg5zgeft7den6mnlnrn0gd6zmq98gcw2w65a84mhz0nn9cu2kp50w059m88jexzl3lqsl5p75wuxzurkeu3w7lsdrnqsumr3rmamxuhxxwvzkfl48scyvyke6myuezy0cd3tvysu2c5xfzygd3gks5qj7cwxdxuv072pxk3fmzjf0luhxl8qdn93l4zkutm35vk53y02r8vjg234a2pwqlc8ky7h0lgu2x27jltml3gra5zjjar40ejd0dscg65u30mnfxjhsruve28rrle398x07gg626fgpdnxn8xe9vsqpg5qlqhcqzu5xhzgjqgp26theygypwnanyf47pxhw4u9wegkyeywshe57862tyuwwej63n8jcyjs4m5wufu79yavmf52rsr8jz4m0uheugh5c65r5h4rlchhff57qgspn7c9dj8w45c95457rfjlpym8axfhwcrtth4zm6nnm0xujh3xn35mx74mc9syu7yzxc8a708y0afk0rlc29ynaerf70f33ldg7du5e99au25p8du5hmfcs7ekn6l7xz5aqn2cfhk90szgsm0lyffgyuay0jr8u7wg40jwtfvxrnuz9vrrflssa7gjhctxscphh2xjnd522pzdkp6g3exz4gepvctp99nuws9vn2t7609ltwrlae7zf4ckftf7ewp3m4puzewugcwcd0nex3etvpkq9lkfkumtc35uhaq0wf937e8h0ljde7svvengc8sav04k3yf2jqmw9kngf0ff6suer93pac47ywrwgml8wjtcf4p8t2p5xhuqzye27m9e00ntenqrud0n39e3acr0jy7su79l4xuv6vvsq7t5ecpzaapflpcyy8rnwdjyxdsrg4hkzrmj008hkdnwpzk5ehcfawckrcqv39p5hq4lcp8rxrau3ylwp3hz7qf0n5g34n7j4gdzprs43u3vc6s47cfqy58477k8hsmw7tkzt0dmj0w72je2vremddlcarqfrzqf5lgpn7pu588az0n97e0knzcwlarxg09xy55ys92hk46z3t48h07hdztdzqvjkzgqks96aju44vdymfp2tqksem2m6k9642c955jtdukckyt9326zu969kxdk7lymavcnhy5vpnrzgrq00vgkpre6lqnpzs25seaa7w58c4g258mcjtedcv04v7lulhzl6z37t75apxkst9uxwz25l8tujuecrjy0l8wqdqym3zw0s943dgcrpwpgc2vv5csnfccvj9e8rjs9fpx2vxqs0z5wur0s5dg4czdmmk67jfwhpnnk9me0cghg5audw4dcj2cd70v6k85gcrkausa7yg6zacex3cy8hz3tvmq09uc4a2jttneqte8km789a3zyld0v5eyyvmaxz22s9rdnv33dwjalrjlyzu3qrw7yelh3exe8q24gw0x2wn0gxg2puthsxlwf84tuq9xrslg7f8fm72dzc058x6dhx3yxckx034newe429ptx3kdzkl35ked4pdd283uepysqmkenulfetx79gma60d046qz8u8ntdw74n7s2mzere4cktuqzvss4aag0a2g5rshkqhtgmau9qnnj24vdvzq2tze4tjlttjj7vqzxc4y4se86t5ge4dlscws7ndptwc2af8mp4cuseeu0txtuepzng6p0qwsrw9s3f4xa4zq49037vrlq6f9fcf0ggtznr2a2epjdgj8cmr8ka650wyke49gnff9pqeal4mzcp572v3jjpnge6w2mqrg4lx00rucrwvk8a7jvx8mj2h3q77fueecvyq2akyp8afsfkjc05xgswk5rec97kymwakpxey45s67vp6w63tjuvwfney470xzrez72rvf7qgavng69wvf9azgs929s0vnnx5lp039ycdvzxpfrnz503l8mgtgppsm49j2ulljvch", Network::Main).unwrap(), NativeCurrencyAmount::coins(728)),
            (ReceivingAddress::from_bech32m("nolgam1fk0fj8zu4h4f58gmpx8fu64d9czkr6h5mdmkdqmuaas303508cv6xjz8864daz3ysnqcddaszmkl5tnnwn5mrra4eqnrpm2guckdelm5y06wj4jg9k6esvtykx36zwt28rrp32g5ncjjtagnh277ewqllwk96t9ys7hduerclmnmks8pek3j5pq6xr8lpmldl08k0pn45j6mp69gc36093mrwqaar2nr5quseuendndawk9amlea3axcke0t3rrs7prvfndcatyn53a6wwmqdcffwegtfsuupl3h49w6f9texlwqfh69ejqxnq3dy0syz000jzlpndswm3unsxe9d639t3qvp4r83c5z0cdwetp9mqkf4svjnsj5wpntkddkx0ce8vv5qrj40xug7netnan0mfzq4z0efrru44usqk5gg9j9up78mce0g4ss5ewkgyk6gdykut508jdwgr8y608krzsqk6nt977a9mteva3m96sm089vqf0lwf2fk5cygttp4v89q0aqh9can6nk3y6pv9rxgwajajk6p3swgg0w342hf7wfdwpdy395frfjamffjcztvzn3a2nqlzk88k0xxp82pss6f66f7efln65p2rpzmrdj9ddxm05gyepqgwqcavae9pvf5785p2u9yyamnw6ncy0anv9nzxu8q7zfrvk2xqtwluc4vjtngh0xsh4n4mvtvwzr94h8dr4svwqkejgzdaq09r8ycejhl0fj53qdzjyvj9ve0c8h75uvk6j2v96n44f7gjpqezyz2g8hnc55hk4p3wkgygzwrclr2u0su279a0yfp0umymdnwy9t264pxm3fnylajq50f4z2kxezza92wtpef9jthu4e68crcmwj4rnmuypyjgcxnj7vufr4jsgzwswczerajzpqdtkczd9c85jla8vy99zc6479sfkd9q9jed0s5yaesdx7jz5lrcky0mxvhrel6dremzvxvlt9xdtmclqedt43g5pektwzw0r5ga49y7j2u9rlcmdsst3ywueph4actkyvy0eynh2dgpc2g2h7vt9ddk5qejnw64eyux0kg84defskfgnvj83as0y3kxaxz5kp7ansmpxwv863phpyu7fp82mf7prk0rm4ufkykry45ekh7j8qswf4px60t6j3wjywcp48axaaj3pgt8waalykjmtshq6dykmrrw79pxrv9x4eu8f4n04vf29c0u5wgma2htmzpkg2nhzfjmenva3ln7n3dy8c76ffhcw75m84ftkw9a98maz4rzkavsj6s5hd633wl83k854mlj5vft0q0us4gtnu9r2qnnxpkml7yh9e9zgp3qv7hqz96zhemwqy99f9x8q040rzwnhvhg287kq99gkhx804c9suqtzthrhpl5yw07yhh7jtmcv90q49vge284389c8ufa59er5zn7a5yyz769eamacygw42maaefzgzg7p75lknhk5339kv2qrvcsphtz0e2a5zakf64xqajmmkq9p80aup5xt48thyps20azzpdttk50k7lfu6qvp844wwxcdnwy3hhpg07jyku0w26adf24rqgdwcddyzxel027vw6xul3gmf6hv7dw8c5n273067aa7pw5jfjpzgu598wczuc36ks3c3kjf8kr4v78erxgkg24zcfkuzlmfz85atdm2kfzwkegckp4m28hddf34xq9dgmarut5jh5cs6a596qjeag2yf7uekc3kvvcwwltznuwe37m575hx0y0cepaspqaq6599tqhs34k3965vduqew0a32uccrhppyx9tzchqnhrp4jx7fw2f9ady4mntze9j8024skcsc557gwfx4nsf06jfa7z5uuskrmk0tfq5pwd7vnpydgzr5ndx4st8zncuyea9agtewyxzwuc73p6gy9x0r7k3zrnmhcg6mc78m8ce2m392canzcfpss26s0jndkwuvqqwt9zge4zt6kcxu9hu3cztxdmvnh4q0fm86ewxds20snm0s86wfzkfkpq52mwcpeml8m96yt3urwdq86rgyv4wth6v3gd28y4cpa59ury53u363tzt4w83apaww4lk0mv5xpzt84t7x25vf3mdumcg6vfna7yv6tnlwxnyaqzc870ymumd0ptwk5huftmmhshzs7hscgg3cq7avw0a7uzzwdvuc7rgj4v4rpplsv5darmxtymwtyqrd3hm8wxjxsv33wmqykwmf42sr3exndv9ce36gr72vqpkjgkgj8cztey7gk9u9efeafakgm76p53zntet3z22hl90ey8fwh22kmuw0rnxmn6wt4kapcwljaqkgzkwt2kntex6e44rmy5ra9n6qc2ezypkqu8zpuvhshn7zrvccukj600ptzjp2jhn9czemldjdtsy4kdmtps9vf5rtsp8ft7vqc03uq9nydlfjxuqktk6egh5zfj7v0qtv4xc8wj3jgxhtkuwfjsvmeuw76fl98n4kv0eh5yk9h3hm0e4ygd8atrnp4gpqgcun6z7dx086w27qfyzu7r2lun6nlw85pe0atvgz4vkdw57j90rtqz2wz89u006azmfa2cxfy7ngen5n3q8n0xqmux458cdcd8uzkxka9umvazaz2nzekpuycxrxq8v6ya95u4xkdtfx0046d6nhheqka49fqzzfatc8gf73grunp00mkj4cctwtgf675xsve7gcthsfgt3cj2x0cjhh9cm5p5v37chd6qnfr4svru77ncu6r0heyssusrzqk008pasqgrmgvv4args7hdhjz854hcpt8gy0v3ze3tj8mztc2z7ccl2dsj0757utaajq7nqkldqs4tjm2rypww59p90fgflr6am990lnxsvcm658we7k80fls4aujvy2su8vpk6ud3jhxxqgmt043g2x86x6d67kp65x8rr6s6prgmgw4zer4emcx8rjc8h58m0l6wal5p3zrd4wwdjqc33gj5k2dm0neydmfesm6gwa8vu808fuypm5lntcxfa0t3p0gtt28ggep6kptcy54qt3xgnqqkzj9p7dpuj5j55448je3ueazvtzfmfywfs3j6w226gangslrtqdpygq8w9jfuxxh86nlkda7xdea4uqc6d2z9psylp96j87j04l4khvk5lrgt2ztehc2d9mqzkhm336jfykhclwfnaqqyxlyhclyv3p0m7st2l5hznr9c59ygvs64zjmjjwefd5f8nydcxd6tnhgnx3yn4ctphf6rjdkf62jwhqjt67clcycya283gy0u6q4azjntrywrm3638ngp6g3aaly8vjzmjwwxsncfzk9sua08glv0j4ndfct0a3zak0ck99akw8l9vhuknuehcfa8wc4zwzhwzdqsw0u0gswrl7wwrapfdrzzgf0c9n6lr70f30ta2wyuea2z70xd58hu9juz7hupx", Network::Main).unwrap(), NativeCurrencyAmount::coins(497)),
            (ReceivingAddress::from_bech32m("nolgam1pxttrkexdv58vzx42kfwk5h0u5rl79w6vfgse0f4j3nge8frx7hvld35j9wwjq8yqt9qckkm9wx33guqfcp0ua2w62gdq44nl9w3umkftcphcg9hm6yg5kq8x4ntdj39efq63z9jstlsxmcap98hzxz6377w4yuv9ae3glwszfjyty7q3e733lmaey6535ngnaulem9kcxqgplmtnk7s5z8fa32k0sy3zsfq5p63qgm2t6mxg8y7thvq5tjxxfqtswr69xrl928yx7u7y27suftx98df3x7qzk2ls5zp6f9e4m7kxwtck8wn78q5w2r54k6v76slggpp7sf5dplg8wpqtd5j8q570smy38c3rq209acygw55z46n849h3hgfnwqz49g9gfwyvvemrv768mgqyqqhlgagvzfhzdklmejl5wdd80c0lxlyk2ssd8gxpwr4damcgw47h70uz475jat35p8qtvv8ss2zascuua8rz976fk2qn8ntkg78c282jsegzgwtmrt6f86nyuqyqq5y24wvzhk8q63xlc8ax8f36vvqucuz3u9lzrg90dhx3ycxdxv5eekj6utxsz2nx59guvxf6rtzvv6ck4hvfjh3et9ap8etevms8yzpyytswsyu7tlz59nsjamtrmt0j53ppp0qgtnc56qysvnyunmj9cdvhd82r2fn2a2lzqcfjc8764ys66ufknfd3rscvdn0tmahem84kryy4skth6avg63hsvh3ahgp0jhl5s5aw7zrrcslaswvx6w7dy3h7ggfc7v58mfyzagk07a0l8ul0d88j6t4zvqwyueyypmhwklq79y44qy46r4529yg2jqnrmegtmh24t822dsdnhxd722z2etpfn9k0jzdzs6cfhjd6mytxm6ca07wftcrd2hgfwx2ve5elk0279mqmhg2fq8ffk3unqg6vtrsaapf36spxj9d2uscrw0kdvrxwyhxjr0vx4j2g5p52fa9ln8u9xrmev0pap9dcummkwwtuluumkrznyzpjt24vnf4955xm9d603lc7knp0ms9cra0zg0a22rhhp23quzxvazp9mrld4llwr0tu8qmme67atackud0f7qqt9ceg6r7m4e4nerhekdar03u4qqrynenedwj6ewnfe79hhwxh2epa0t2nue4y4w3u34sqscuav8jvcvrd3xppc8vmqp6x92uvf96vc2sx8f4fn3r8tqne3mxh0q5j6zvtaw8fu09plj3mmjcl7d8wcr3m9xc7xsljamklyxtyw75cdsz9hnqn86j50qk959uc9wxsqkugxn7ql39ljrw5gcnqvewvn85d5e4uugkd426p7h2rqh2fvmltd3p8gt38s7tx3j97c0lm73uu0ed0luty90pmg3pd6qyp96meaytnf3y528nx4g7mz6xwvl78s7wtcqd9x9z2xulm7njx265zg3294nlwzqgsj3mskqhs87365a6lwydkwrvwce6q6c2rl047vdnxr728fdwmp702lu9dfjrxr0zgkamzgj8j6dfjfyzdaqrp4nwjaqh8xgtgamsw3y4zjcuz85c5fg3wsffm8lv320fer4rgzsgdcntxcs4mtn2qdc9pt6ps3a8cula4hqkk6rdl3fvzazuymwvv3hcm6f073ffyl6wuxhfqcuzza4avk90z7t9860xtul853syz9de8rpuyjzs7j8zy9gupm0ymsxn8cztkf2p5sxr6r0llsr83cy3u8pzfjxxqzrql4n4fkav8mv90s7u54uflyw0s5mk3nngny648z3hnv6m58zvcgadm3ez4r2lnmud47ce6jt2c306wwwwrz26er6pr5vm26l3mm29s85tp68v6msp4029zxstu86r2kan5xygh50efmldlcncnqny5z4ahjf2naxte899c05ex6kflv8e9g3nsr6xe8lkszlx3lhjc3y5v9hv9p9ct8r5eca46vj9gh5ccknpyj0f26zzmj3j97sxfy9k8k0wkapnhzjvl9w7ef4jxj2a08zcfsu0xfw3azu730xpqfc5aqfv6vlylrxsykahuqe3nuk66xk9sary7wzd6gqcum4w2lzx26dyuqxp84u5wpls4vcck4wthamnw32440zfs337gfa9u77ymcznnmfedlvx20y7ee43qxvf6qfmldhkaty4ev7tk4tk0nsfy822gw9lv2uj5z7ntpp0vtxjndkzp9rydcn0xldj02a0j84t5z5sx6gwctvkkjk0xq8j9mmhne6q32y6sahp82aqhky266hcp0k2us9swq52welyrdjcgd6c6q05cdl9dk54kshyay3ylzpl0ll9zvvdswasezr4s4mqs8ajq6kluum5vfm5ujypl9kqmjyw54jccgh2qkgw86e9cllau90ej0k2dj7q26ppxl4vnnydp5vsrrulrqhjh2kxz0e7nrhhcztylphewel9qrshmvnzq68z252vf4ww4kv4sq992d6wtcdcsvxrnl4zvfen2xkmc8c7lxe7jv7389x9gfx0wq7ykh09fd65xl74kn2vta0tmxmspfuntv304t3h7mhffgrmek3k4g77lnq3864rwd92vg8rf5rkwdxeq244q985ftf7pypkldfzgscpz5k9qsm30q4twqgga4rmet5q2mrlt2qunx9pts2rm55zddvfzqu8w7x9pfrkfnxuqnsayz8kfmfp9fww6su0j65rf2u6hep59735eye643ck8argau56x858mkmxtnhf9lgkg6ysyajgu05qp4f9qamhuk0nmm23t2l8ykkcudckdwzahz67jca246jmzlqck2nsm64eazv6jkxq2v730kta6qmk8xrnglx0wykwmy98nenk9lztryzxlg7ety3lj982ztjucagh6vzl5hc77fe6dc9uk6kekxz6hqdurq8t5e4wa8pyznu2869eszzrskwm4qygamrpcrjk9pkyyaxem5w059469s6x5npjfjetux7hf7v6gm0ne0ht42y5qj6sdyrrnkunwgh2rd89xxm3kede9hnmaj0h4pg63l9fwm7smjwakuh053ca3dljhtp2xt9w89f867jzn4jfv2g077qycaxsl53wdt8hvqvv69rnl26h9uhfkd5me80j5zs0ae2c4x6p353fuuf8uwltvt2jvcchhwa2vcgqg2455x8ecekcgejqvln8dxgus05wgxp0yz4su4x6k76v33l9w5x56g8fz8ne5xpnzp75z9204d46p9hegcjrjxnzht3vzxdzanx5p37ug60saqzd2paesf9s6ftwcn64p7x8fheykmsqy8r946tlq7lmldz9uuqtugy43sn5ncpfdwyjprylmfwp4pr709ng4al06hrmpce8euza8hwy0kla50gg0zm60nmn6t05qfkhdx454wa3k5r2xl79524zxwfpxef", Network::Main).unwrap(), NativeCurrencyAmount::coins(167)),
            (ReceivingAddress::from_bech32m("nolgam16spht7xyenevfyecnchr67s8wplt5lxv7g87hq38cl0hxpv6fvzkutd8s4xu0cqxvs5sa32ahefuxccdhrp4f0yt5hhsn54fmx4qtjcnzlddn9nhllkhgma9qazkvh5fs3tf33gcel8zk254n2985jwv9nlhnmh58qxcjzj94tgt4eu7t5pl9xeu83fcj7k4j78g5ypj35efqsp2g3hu7glhtrqku0muqzv4k47g075jetppzhpv9rh8lc2756wcv9efwz6t7gfttrqnv27zfvghjhefs02xcgcndnjk03tsdxcxt3mwmvwmas3k9apkchtq6mwjzzz0qpwexvug09q57m948kt8cdpk5eg63pecg4d26tu4xd7decheyw7s55zv0hfk09a27s0m9jxtrrw4d5tgh3y06y6dggq5auufag7esezuh2mdkp7vqzceda06pjtxqxr023mmnrq4kazl6yv0h04p5ay0905js7ngh45taks4n6w3h6euf2zv724xvkapunr8n93000s95yx80kkaqtkukj83rrzmvxpx98fuu5w76332a3uvzfr0wl53z0wkf0xcvvdg435rdnznz9u64vgu9hfskwphplvq49dhg044pkzjc09ghwdda6y7f33fcgxx35jleuww5h47pldah6yz3c24gvcptxydshm8p8dkr4gqg82gceylr67mqfawps6py0wzs9rluz3jf8rqevv9jfygdjzc4v9z28llv8t7s7a965yhnzkeeqy9kr4q5k7ufnjuv6egqj5vqv7620sn8fmzc8tqksqyse4em6t4gwl4nsq25gr43l7tflkf5equw4k83crc9qvpk08nteuqxsmy5v00jfehk5h4n3nr9rm4cdt5smf7dhrdx33ng5ka7vr5qpd9ues9gzvzmtur69pkqzxrppgn86893p0tukgepkzp2kme53vdede9tsgq7yfthstsnnp27lwh6tm34amyvuzad60dslrwvkctdu30jemctkjk2ngft93yh239p3mwmglnv30srph8927ejtgerqmst6et0f5d2eyrschegkra2jyp7vr88damf3xhjstkjdql8ms5arzunzxslj6thwsa07v33c90uh50qs0sauqkf2lq3at2gqnadssrln03y97fvqhhkh0m7w5w5yxc5wjr2z3zecreq82jau0r6p5vwa9fsnmu050ylj0zr5k83fmffwc9lxe6unkfcd06gktps2f537pl6zjmettyva704q7fsqyj9j623aq8g5zj43wllmayq3ce5atqp9eyaghh76smx9vw4wkphncqulukqfjaxv6tyv3x762mhccuj0jjtxrdp49kt66psqez384kgtwf5amrtllhtdvedu73fkalfqdt29p46gv7krsvs5rfhdq6ccm2sx74qywdg665afptqlywv0y8wqmqfnh2mcznm9yx8v58qg5zsx659jytehhy2w9trs7l3ywundk7qzj07vlgsdp0030p0l8scy9dtf08mjjg47tfnnhwwv5sezuzrsu0dvhygt5rgufgmcswvgfnp8eztn77upva488u7ce0l0ca9pm82a76yygay2cvhq36x70xu3zlu895d4dnhy9gu6qpkztw4llvxl9ugwrv7wrl5cs6eadgfracx390yxzmzq0u96s33djk58mu0x9v03rsmww24dgrhcypygxza79xg5dg4ykqqclws80y2t0llr9e3tm649wvhqz8t7rmxwyuklq07ufwpu0ekjzsm9lydjhjc0cjz5mueuq2ha4qv4kj9em87shnvz2dc92huswynqf8dcfttdc9zymv7z4yyc8k2e5e953tt92swjc92fqq2fhv4vfgryhqqzzwd09vq9kjkuuqw0p3482v6f3062chvsp4dq9w0z0kq7lmqu4q228l6xrxqt6zhxxng8w7ls7g8l82uan8ut0spfs9luyw3gvph23qghl8nzg8vng5mzna2cre0c8apqwvdt3ct0usc802rn9h5369q2afvzlmnneanzye8rv8ae3wmf2ys2qr0japruv2vxy4qwul0uyhzzmxgvamd552dvhrs782mwh2983qfdrksz32nm60tun6yjty8xaffq0gnjaeaww4y9fasmxqh5pad9jfxtepffrxwtdl6uh2g85qqzmcnd2v4crygwm2dwtvkamn0d2rrs7f6wpa0vsjpjqu3hxvansws3jsncx9ku9whkkuxuuvvs2c6tpuzeny6urekcsmrjc0q7mw7f5nggsvmzehe73mzy0cjq0p3z8llcr2wdek58s80jhda00vjnzc2xss066aun5j8ck5zx9v8xc0m7wyq2d8xt5rnf0zw3zh9fz4v6mql8aycnaxxtr3e3uwjqes8gf0hdt6cl4eyygeredwwn9rsfglwpf6xlk737enu76d3rg32y7tn2ffj727g7dkw3d22h9k8zq3ezegqjh6wr73rp7xv7090m8l3ctcm7ery77k3tk0l6qmdr8h0r0tryns6g05yncprt3837ypap0x5m6ekl77xyq6ql30dlr7fcg2rrc4thzrs7spdmgdrhydcg77u640q24myj4v0a479dqkjfdxhg99j56vjdzwwsa4rzxharf2t40qlvkgz727lhkvu99y9y2ju9xvykxzm8zaszxaf7uazr2tcdlp387mcmy7237w9ltsm0e4m6c6xf9actw0yh5966v2q47m858tf9keqv4kp98yvhsgfjlh0nd0ym8yvmgwuazwwpgxmmqcdn58zctvv8az5rm55c3d4yht4nmu63kvjuz3qd4r9jv2myqq0vz4k535sgfxllxurm4v8y0kxydnv5ex07nuh7ssufjwszenwuekvklz07st8rfzsa8fqwz3pdrtu9up4dxv7dwuu6ghg7aee5vzw4rtalcg6hdn5qhg2mv6g9eh5qrn7j6ymum5jnedtl7grs8f0jjnhd3lhcga5yvgyhg62swml9xca3aag7m0u9akx0dsg5gr3pdras9uy65rgzm99ft6myhstw6srv5epqldjv5t7h5dzagmc7rh0adk6que52glg0q0hx490pt4ggppxypjeu9uyamquyas3j4qa4azhylpg7f4w5gtj28x2pupcnsh6tsv58uj4pyt75pks5d5d2rre5vdthmdcexpjh6s5r8fhvxh9s4697v34egwxalv2nvrn7sc3h6q9nfkvdh9vd7dy8nghv2h7mqdt56nvpz0mfsg6g3gyr7tmsr7pehdz8dq0we70k6uaxvcp3vlnxnmz35gr5kszmkz279ejn6e29gjmw298fv8g2vjv766pltkwr8q95hlmexnxuhwjm4ryj0fvsqzj6xjrn4hwr04tx2nw4y0fhnw6q9lmsdpuz0xalrc5puwk0drgz5t282l9g8", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam16r6mvq6vpft07wd9vv3096t5upmx3wqsvykh0njqp2lpq5kqy3us4wxsskywatyztt2wgcgkuwa82sn34ruu26u6vrdfa9s49q54z6vx9pefuan2zfsztpu5pf0lrwzs9u466quesdj877d5mkq5kjvvl2edqpwhcshasamh0zxpmsn5nwl82l0u49ks2c23ehqetwrqcwgycd5f9j29ds3uyzn622wr4pkl0pg0alxctzv5mvj7ug30jjpz2hw6xq3udug6flnfxpkgz8fmm3vztlhqq53ffm2cs8uk90az7d7l8vyltwcwxwxsel8k7mxz7gk4v73z8nk6j56qfall35h3kq4ep2ezl9fg8eyuuf6df98er7m8jwk3v64fxpuknt7t9dtxnd9cltdtyyu3l689y2mtz322kpdxhyqyk0tn0yrerh2exvkyqklulr8xm42v5v3fyvhxj2s8r2p08dpm5d2vst0r0c7jdl2rcjk0hdnygrt0ggtg30w2gflqz9jfapjfzkhr54tfng9kde93cdlyxpsalznq3z7hfjzlaehrqt77dqnjvdxsja0mt84272djdrpr4valv8spy62dat0upg2eqk9ltgz5afqgf4nugngrzqg89p0hn3r3egd05l0vrchjkrecnrqj6f2hrv9cfpauxx4x5xjjml2ptk8ekjvhgalzpg32qfkewzsjy56elcjzqj26u4wel5r5jzp9se4a3v8cl4w0ufzm8c8gmk3thgcht9c5cyj0ck2gtpm6mf7370qrgq6gzpuczn4shnxhc7xu6ypnx5dpyvsj4nujgp7k0mk9xsm76rww7x6gw8qv675h2vpgs7fxda3pqtl93cg6kakla7p78zwu9vghyg5a2wch4trj00hp4a56crvfjdmtmmypsvuqfxn49gac2q48yjp8jtt4tnafgcnq5evyv7zpc80eedudmkuuyz3jq7auspg54updkx8qqcs0uj2ezdjq496280ww3x8sfchafcztfe4k63vnhr8fgjqmqwrz6axqf4c2lg87n6wxs02x6675pvlqep4mnnmeq7rp2muhkqfdt6ln333s4vhyuh8lchggz625pvus7au4nalmgtc8rwts8345fu0fsu0rdgw92yn208af6020jkg6mcnt7hx6m8xjnppgnmm9gdy9r04gej5cj7had4m4wdcwrphtp2v27sa9n9lxq6mhpuamwvadn9kzhcym89cu99x26vfjp9609md77qeg90q4fwe5zaehxwkeszwshgag45y85ur7cpe98kzkt906629hv5ccpkxl3hsrxdjrdn9g0v4gmfvperzfvsuew7w3jdsvvhkkeqlkgu5kvhj2khfvwx7d5lef28q2ng2au8dflduwz20avtme42ghe0gkdd56ljygj6fnsg63399e6r3c5ytqjxft42set7vvzeu3k6k946ppntspuxpkgv2n8hh7kpgflcjyen6vv5km4j7zgkrc5tpnwrdeh92aadpvtqcfnc8lrvt6pj9ugypdtv9krml6rs9tew7248lur0kpukf3nrpadxv7m25ats0d94rwle8sfevely5e5gc00ztk08e7myyhcmh7xpjnx0jay53msls3wuc800vw77fnffkrwwmg9snjr3q05pxkjppnwpctfdsslp500vqmegd2cp2qwh2h0cateu5r57fcsnjk9yamcvwesraa7ep9mzl25remuuc8lpegpq88e248v22879elrwpwn5sc8khqkvmyy43yp8m8w7knkrd0zn3jdh7s2ezycm2wh5mxqnccxdhu5uuyz06jmaw0fmzquuts5a5cf0nhrqsacuxcc0jq6ef4kdxd6wk9mjllyl77fqc9qlt6z96mlfg9ge2q2aqme4tcuef8hceud9ja737hmd95grkdp5tp3h8k3xkpphtng6wdnh0eqmunux5uyv6eqs2rhjydn35hhcx9kz98u7ykphsq09478a3adqv9n0dzzg8munwxgr7dc9g47hstj0nrwr7nt8rpj9scgsdthw6kteh7dv675tlv7mjzewejzu5au9tng5zdfksjmhgvgg65xutwd0zfmhj7et05u9jqzgyxaugan29qtt7yuc9gnntk2n35ehrelg40nuv0yhhg9fv4phvy59gynptjk67v7uqt7uw4myxjaezy0s00jywjy0sfr8mn7nwq2snrg4pqdrzn0g7ep33qkl5zl8ydqr7gfeuafvk8weulzy3e95ws9a25znqtmu2j9x9azyaxgeltdxp5sa7zahhnpl0fvqr2tsdff65dm9v494r4dcmqfxkkr0ratarct3jzlnnatgdkl8fupc00ahkaca0z0c6qvn9srv0qtpctdrk9uvf894j4r4zj6lfgt3sl5pzsl665fm7tag90yvwzwtly4pfhtuesfv35na8hm72ecr6f9jk4fyyhntl3vq3gn7x2rwvjekwq4qz5grnavx5lznfarr7jp8l07rar5edjhcwsxeevfmdngx5uwpw35txpf765hk37uxuguejgrwmpqy3myrcpw5952dx2tsz8y95hjzhh9m0pjkqlg7ajnl42z8yss9t50kle6l6cvfhc35sh4phjga3t80d7kcl5axxedx275j950hc9wxfy0ccyqqh7lv8q0wxx8rx05yrcg9a4ud8z8vyphu3cf8gz726x75jjdwagqf0ayk2zmahsz8thytpp7y0m5jh59xyu85afeqw02wfrx8r7c9wpvfp9nkgaxfcp6atp0v3ydz6elsu78k9fx8zh9gdkp4lk9hmyhuz3kh0xrcurzn4l026ex3ed4k5fj8ryf904ked4yw9rpyqg8fjk95msjlagdxd5tha5x2f9vlx437sfqjp55eu5rxr9v7a0hfk7daenx324jj8s829ssfeudcvt2qgcxamhjyx3uvdmucynll9478uj7t7y9r2fc7nwl23nw8fw6p537vpfjrh94ele3emyned5a4gh56cw6lz42d7ea0xkq79xsxv3mmx9k34vy04ekhmh3q879reqecz522ctcynhfcgk723nxx6duyz2gx6qene4n57rusmkm9hg0kgm7tzczhffylaxnf3vg90032scg67qnade9csvus7eq7l74qjtln53hwjyrcr8uf3n2yxpd95kt8hm55qz5tshn8up4c9h9mng0pwnqeayew8mwxtlc9et5kwsamgejnw5tr9wkeekmfmvqlsy6ccvjh53qccqm2j0ny66d8uqanzdq72nu2cyk00z8ckp3snrftenkrrkarpfc7ty2pycy3pm2ln9g4lzr9qmcy8wtmdnghwgkk6kzuhrkpmaptjus7rnrjw3vtf2yfjrpycxalh6ak53lnes95hpqj8xkt00s2mp4enxlvhaf9wpnxsjk62ejuew8sh", Network::Main).unwrap(), NativeCurrencyAmount::coins(250)),
            (ReceivingAddress::from_bech32m("nolgam1lvnxrsgy5hegsykc5p768ezmkcmkshkfzlc3lya48pvlpcn2r02w9kuaqte7uxfp4w7948epuhjgglyqw3wetlz4n634vjc2urgplvw8k4pz4ewym82f0l5vx4avq54gm6f96673jmx9xe5sfq2unp34rl9lvremdwh4t4slyp7882zg56y3f5mm9vrfd0urtupznprydm6ycpauw5mc6ktxxct4xgcvggpdar2xkujszu7v26zc2676cr0h5cdlc5xnjtkhzh4vnx8dhezkwfmxerrvha54prg300hf5v8uj4c3p2ca573s3eez943cq6t4k2zy48axnt5svtwn6nusgrmx7stup49sh4w2s64use6q2wwhef2ralxe5mn4063rafkvdejgsh7pl0l9f03ej0unqarjfrh69zqyya2y3s9ncf45y4nr0en3f7mql2smsv7mjmr9w2kdgl92h8lqj0e9gug9fth8yv7me4jnhshnupxvaxyscs8ycyq0njxwk5mewvl9kkg54vp8da2r6qqfeujatn9wcsslsupleknndxnkvpjnxq33mpwy9w7j2zsyg6xq8fcjjl5xr5324agy9xk9gs3v23zpmdw5rutnpmgpjs4ahsgarh6xf6zghjaayjyf3rs52hxnefngtds90a7wfsz8n86stp0f40vl846zuk4jqugyyhu5j4hagqga72zjss83p4a6yheku3hayx4qk23e5uluzcaktsxqv57r0extejyefrj3l76f0znl823vsqe0y5mlzcrnt5mmwwvttesz76sdx3xzkwslqmcc0g99u35xtz8fcrt84z63rkkcn2umk669wg6anmlzpk8t42pphpqt5zetafs0vsw0m0nrsmjlcrkuvd6gdjgpgzcnnpzttu0kmutg2gusyf28fxvuqtt9whq6kqygxfmyxrs9mzzx9w0attc7zwr9w83y3lylm0prjsnvrs94g2678ax3xdzfhk0vdv2wj5la3zwyfdrat26n0whqeh644vypjdeaptlk0utprjgpsez44kgnx38wpjmj920dpzqzxmfhvwafuwg526sdsdfakqapr4ca2lpdaw87ahcyc98d2pw3s5ed6sknasyn220e85jswyqz3hskmpfhj3p8rewxxq5ry6glpmvsnpla9436mqs354x64saghhj0qnwxhmpwk8hehrwez7249f5l8khrwuvtyq4fe7gg9904kfqvvzl87xxg4rlcwxezs9twv0jde627y6a5udmplggyuu78h427kc0tn8ss7esy5xqv9fwzpqykm4ezgwntf8q8lqf696regpmqrad6wp877hdmdzdte8p68v7fw8quy8ed47lyer8qtk7zzsp5kh8vysvzfwwm8xxxlkqj389enw6eq58ymfm4l2adll88cq807gxma4py5c42jyadtyqej50lunruk8ta82nfhnuyvq85cu8r559fwmv7t5p5xnckwf58nr320acp653s4vchk6q2y33m505qcdzfegvf3cwenkuvrx0hd36urjqv3jn5q0977tml494d47cartwpz743xga6wgavvnmgt4gtses4ulkl9x70wdg4fvku50wj82xrg0dpqmvjgeznmxhgur3prl5syymmn763c3c5weqn4jv5vfckz6l7k5yzkdrw2u9ekeaynrd0y87xn4t0a4rqx7r6ze6turcm05egjxesnjglgm769dhu24pxpqq98nmkcrpcqqf59xtlu7r0pzm9h7r9qmkqs728pd0mggc82txd6z29v7kclgwlfsu2rqptxwvgx7qm906n5xqtk9ghmm9y6trrwyeyekpwzxap0l7k8t3fewjqzvmt0lytyg65nsu4hrfq7k7w038ytu7l5dus30jwn2m2jtwclxtf47mn53sreext2da6hzxkyyyejampdtym6kfe85zhnx253vha4u2juej9jyj3n4sedy6hhc909ryfsseg9ks8nk49h7at227wdl89n5ddya6q7wj3radm7aadjrjtmqxsf2y6yjkzmpphkhfvgutv9mxjfr3l95h52eqwu9ncfa7hp7v75w3364mhhx82salpqzgwznhnsj6yzjj2lnha2kus8neqackcwf4vx2vhnjshcdeu72w4zhp6a53jw72cf3m5hu26fg7w0dejm8qpuqn0fx0t8n423rj4388zszjzwkpy7kkr3wndqtjnt8tfl946hgehg2htervnlvuk4lkwcudgdzmgyxmdrkrlv6ujfqv09h0eamqt6d8yw65ta79pq77pqgd6w4kltcz7u29c982d7dvv68ytpgsvz7jq6p82lcxtlkf9zf6x38eyjsmgw65cmnlcjzcfctezle92mayfd29ha0jxaj439gzg58xkklxa9sgfl32rmcuarfp8fdz2e846tg78lrkkmz8c0und0eftwa5xnj4gajzut7wdccspjczsluyre4fflfhhcgp2ztzhrd374wydtqrdq2nkape0wjhke52nwcxjyru5mz9f232xu3emg3qlh7l3c0629z8mhv0m44542w3szhafyu4gl7dwq6udnz2r4zfa74t0tpvuqam3dnsy3e2ced9xlls4gsqtxu79skwrm6qxmm8k23f9x64n9j3pehtfffpa7gdneu3zl9wk7njzanfgsxvxdxjxc6r7jjenryzsv67w629e76a94mnpjl864y6xwj2p7xe0vvs3rj7r3tnmu20ycevdvps2tdh3284v864x9jrt2jr7clsmty6shcrhrxfwk4wqe4rjsdgspq9fkdynjjsns9vug9jhl96nzdnzgpelzwf2yl7a9yg47snk55mpmcfhr992x796lrnkhw3u3smjqglpdeke4t66yurqp09j6u8qsc68c396ttwrs209wy0j9t8dgpj27w4rfnv0jpdahyhcnz422dxlyt7f3ukx2yqkfyu3klzksqkzwj90jm207sz7acjyzmccny7vdqunqanyvn7mqdeul5e7kufr8yfdxqru589ukdsuajtlq2wg3ae4fknm4kf8jzfqyz3zppr57q4rnpve8wxpfuqe6ns867awlp7s9gxxtcq0u6h8mfg5d8s8aymxk7uzzk2ar6pzduc9ndpska7px7gxmex7nwy8xa6yp903k3f753xrn2tgm87xqg9vgtqmcanjqzfp99vee427a9hpj82eks7qzg3dfeksk5uxcwmhvzh8evu0tmnx6gspzue04t4zrcgatyyqdjpuvfx6yl2pp3qqyhq238fpdmze8fzyka99z938zsra7en9usfdekwg9jkr3f3c3xrqw38pd9zkzcf55tt48kkqz6cw2mwh53fm9pez4gv8u8sfze9u7pap8e23n6qpw373tc3lr392mkye9cre45knm075mwzqxew3rzrcv24cqxg", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1fu5nxc0wdgtr4wqsph0xwpyvjjc065hj3wlq9va3h6cwzurugppfwzae9642p340ntg5rzsafflu4qc533ydrl2uepn9gmnyluuwzzl2sgy7997pycczmfmyrrpjxef6054g692ekw2cph7tfsp0w74j3k5mtzswnx84fzafrre93pmd2fx258sl4jt2xgtfy5l7mfsn2w0enh7wzdm97rvknmcfglaws47t2wlf2ftdtrnnnt02jmxmhuvxhc6zg52zqjhp98lsddtzkjzaxge6xxvycr34t4agr5ww07ny7hjwlt9gxyzxlm4gu25yssfpfw8tewe0tm6fmwxv790ne0up9d5mgmhzhaq3jldd54un3ucvmu764x3ukvc3lx22jky80xqwpjzt0g4zjs9f26644kuczhd6qm5uaf246ap8jzq26zp2dvllr7u2xn76df4hasyhyps5zrp20as6arf23xdqpuguxj3tywe07y7u0qwlq9flfcnchu39lnp0jfs6p8dq5084j35t8wk2v840h97p8lxmt37ehedk9e5nagalajsree4ta42fj8cgr8cnrqujh9tmcn8jdnyq0smyxqn6uhwaeeq8rxcnc0t9m4xp9h80znc0aexawsmznj5848eys0rvd6wjmq2t2u7snn5trtct9upkyd50r3q3zcj7wxlt7yaqr7fm82l3yvrjn8xzn3jeyp5swaz5xhv9z5nfsuqk35dgw2wwhv4jd3sczqtnax7gfvtnnswazf0y04ynwtz89rckudxyyfkdtp0k6dfz0zv94pa85vf30mljevhytk5ygznyt20nvx2rf3l73h7z4rd2k5dyz87v4py67jd42j7tjplh4y0582pzxyz4j5ur8mycfehpcuhzhq28yjz9h4d73lmel3d4qa5s80c82et66nyd4jt07xr04m056x5t60706a4ygun636usxzdq60ex4ewjnyv5s8gg9c389ymdm5gyt6hyl33ms3jjs6v434ku7qex9kdaq8y7efk8pz237z5m233md484hldud0qdl6ar8rzhyyzadyp8y9vvaxznkrryymmvr6vz4xwq5tw4fxkfmvlrgcgkzgfedetrf5nhqg4cvuslsrm29v49q6fanu338h8wfcx2sxw8j78zsvh2fqw3pjrka39d020x8zz3cg7srv0fqy9klyfemf2k8g55l4s6mhmed7mha03596ya7jaleent3mlxzmvkc6qnld6sx7cu25frlk7xlukc2cf76y7at9aklqdknr4cnlaqqg5yj8wlcyrep637efzj6mllawcs095802e9yepy49qk6rj3utqmemph7fwhv0a63tnhwehmqg6zptakkpyzk8cv2gm6fx67uyqs6w3gtxwv5cv5xey45my29wruv7x9ex9fxnpxw62n9txa8qq463528r7wsu6refmxxwew8asa44j4288rvtqr6uw3gy4q7jdmq6304l0lwxxe54rlpwe4mf8r9wvfty3r7npl2n2qf90n9eut04f8t384fxvrqpwzvw6ny8faqj6raf7snuw7wplt4tvjuaajlp9uz6yndw7n4rt9hazqmazx9ccjpr6me3rqps022x9xeym00a35yr0el33kwzsw3dzudazyvsyc8jrtxe9853djmmf0x7g2nmn0l9murjrlad6gm7xmwdv6n3l34g7s2j2zhlyxldkct07tuzxgmc6jfr46ujena09fzfhs29pe66yx6q49g4jny6zzv8d7kzfft7j3gvefl5wgmhqhp7mu0l4dzqhtwqlpylqm65xz43wlayy42czp677wdct44tfz37s7u5pp0dndk3ukftfjf0unaj5k9xtvykc3mghg2usdn44459ru87wdlaxxv0jh73f4fnk38l4yq88e7nms7geddktwanp4zut42p5vrr03gp47qlxvdwhp6275rvv6cmnkmxx53yf7j00546gde5c3lpdk3hsg0nm0mkygxxgplm0ff8xk7y9dwtusvlrh67suykxnv3zz7udyxapeta6xkkefs425lsg48ntz3cz95xqcdpx424w6pz3z3jfgjgathw5fsw3han0v7g6zhf792c76a9v7rtvu7n7cyceuq4s2mga5y9720jl2f58l09vntns0wrud54cxxyqj8pa0957z9nfalh2k8ld6y4cehc2sgzdwkd5dat7sxemn7u6s7lwkvdcxxqnv8g3c9cvr4e4zdwh53ag9565mj3umax69rd5jldzat97x98djqapkpgsheuc53tjj30jljta3jsu4ckcaknk87t6gq4usnvunae9g78wwpge6due8z4lrhhve82pnmtes2e72t6tkzr988zlslw9n0ufxcexwkh8skc7au28ysw4s9radslr9m8e8ax4rfk8wmvz32hrf4eyc73f0vzdzu7wxg85dfwfln5uuchy2k2whyucy66lrdeeya4gf6zaml222edh085sjn0snqddkmle6ug3s3py3tfsgq4ftg43pck8mcvsydvjdyrn7lk0vjqqezzc3k38w79yzwadl3p3yyhc9ydqkv9txzdjxpjcfdzwn0gg7xzpsxrrzxev7e5phkq3nju3flrz9ufg8n92v4sfj7drx2286h9plnm0648zf09cuyy8eplf7ywy0xsd90t2jr5aur3d29p0zsalg59pmk8666pnq8j4c4vhgcqpj9v52k8tp7y6lmcj3f57mvmjtn8q5e72973y5s8rlxkx477xzul45lzrnpzhvjs9zgmrl9c0aanjsfsl2gnyueel3gegwuuqrekwn8gfkusfvlm08vlsd703vf25wk84hswmx7etwje6ffpf9j3kkzwfeuz2h0sqwas4ayuzk3v4ptgq4wu5n7000v3tg9ada30d8sucylphas5cuq7eq5ww3eyt8qefpn6yy0jtkkjsm6vqgmym6mquqlp2rnqh7390wxydn3d4ywl7cw75vr7k0t9ktpeyxqkf3upz55md86c6w33k9fa2zwsw5ps6t3tplgafkl7eshufsjg7rzu9255rxlswvz6l3p5jhf74xw7rsvz5mvjdp09erhy2e5yx026w7jyxmjchgzfjwc0l56q2pdlfclfpqy4etldmll2s5rpsr6ny6vf9qxa3jeqkuhgv7aekx955f9uxrtpk3mqwle7m0rs3mvuwje6hv0ncuqxt0qucm29x7yls08e648czq8s5cg4heq7k4qpj2e7vvs03jlle386mq94g5nu7f8lscsejq09cqpp0qeurj833048wpgxs4rwwfzfdh2jlxx7qfen9f7qhu3cfzrtle2rkcmv8t82z26msjykl6hk2jyvu9a7lqtk75jcymp4449l6c9tjj5u63hzejdf8zcwuhc0r9xy7a76ulrc6tktyenlmfgyazvnl", Network::Main).unwrap(), NativeCurrencyAmount::coins(100)),
            (ReceivingAddress::from_bech32m("nolgam1w7hs9r9xl0s94jw0c8mu6krw3qrc9d6gr9cufkf2ramm95s3tdyqj3nv6xr8hqwthsnxrrnk9jeww2yjqmafzjdmez0hn2sstej8878h3xfswvygyw7yq0hy64s9l7ak5u5x38qf4ctsm7au9dy449ndm5e99xxgz9pcumru635trectcu8kf2ffmzu4x0u4fyfg4aav8lz6mptqd00jvh983q2vqas3tpkuzrdwy66ywgq333jm44pgxrkgt23s05640jkaydvxcyy0yaghvflujtg37vjly6u7q2yfunmq00xqjmyqw4jupvl3035gcfuz2pzzxzkka79ne0ghw4kf5nvp4flwvak29j0d6d7j4d2222chcwjdg8jl8ptpzlz64wh0lh9z5q0p8ee3kzwnqk5gx0levzsapc76xc9hztwfrhp5dwvgvypjxwqpknjq8nya7f55e5scwhup6p4pwfzaqmzpfd8xv3q44f75e5e70kuexg7df5rpjvkrweyqk8csaxn03ycxsfq5dyf75mjuh0z9aw7tv9rp7lspue9mq3qvgyws7s5uehl8x7ws4wucxt4njzcnf5fptqkmj036pv5rq3j3adzpam49p2elj4m75wptvhluv3x4k6mza9asws24822psaxqu6d987zk3cjqcn675yvquehgqqax420as2j880sa9hrz56dzlkx5y5rxdgrfkrh5752xmmfwe4cyupvfc4gc3rmkrdf8lsks5hwl6pl4m80hfzag4d2d4g7v07xpxtg8jt3demutcq48ezzprdtxlwd4n3h9z8qakek586cqtr5u2uu7mxxf3x85vn26r47802kftylw66xxcnyhxm0sgp6cz0e4ht5zze4vl7qs22ujkyqfvzg0tdf3x63hdzglmmfsg99z88th32ypvl57tk3jt6lkwd9kyegshhp2a4spnln7n2rhmqxzrxtmqdnyl8tcfjfnrex2qdfcktt26jz0682jcgrkkkej78z9xxv0xwflkwfr7sfvuyjw2cheglt6atse2p8jcynxyljf9g73vjgz7czhhe30ycp9vfp0k3y5kcg36t8rzfm7r589ea98s4fwmsfx2m9et55k56pnp5s0mwk9wahwtvukjxnfd6e3mf7s4g6yjkxqqq52gmalztp2mj5e8n2gvn5a4wf422d295w3z6w2x5stsdln03mml95m3nnwgtdp4rlf7f8yqnylqegcjmgv58w05fsxh5v3sukc3ltgl9syzlgfptddh8xvm3jm9hjhmp84zfl9rum9c9apqzc6nyaznwvad4a8a4gf75au9kumy73fpe8autvc6va32577n9z88dhjm8r5fudz2pjq64em4gl9uav5wjkkr0maf2xe5nlwl3rc59n6r3exksevx7jaxmtc7feh66s2smgvkuvg7jvfpmntgj4ymqg7ra4t5uvqh9szmgyzhuwp0r8pd8srg0zkqxfwjwtz54n0qtnv206xvzcypy7nehsjf9hmj2pj7mps800ndn0yxkv597edxwrfy8wxk90nxxv0w34ddf5r65j24hemr7ycenpuykwrqy7uvelycek5h20q7rnhlgtt5n0xwqyg69qagg0wnrs8wsejzl57v6lvpcfgnjmlfdl5dkjpcsfynyf724lc5uujvjatmrc0whq5ud2z72j5czjugamyp8wte5wrqedjczzz2awemctfu7knp49adyk6nptcjh73jvz933cknzsk7q96aujmwfg73gk3cxjcp05wnm82t0m40c46a7jv0a442z39g7pjp4xyqv9fn2hf2eq4wfhduk0pj8a0c90xlzww35u0flxksghza48s6nhlxg5cmunsdsq0c3lfmgspfdr3vt7d2sm6utqj0q9gr4ua6sn6q4akz5mzg0ylwrg6ws09mf8ekzumnkqth8dqgk53sh5npr3664xnnvmz7ef4jryvq59g4lx49awyvqz5fz9mpqmhsk9rn67f53z4x0800z06xhm450tpg6j49h7w0n4uz6lr27ruq6vc80xg42pzuzjjpe4mp646eleyzp5eh6ghujdsz9p2w5qw42nqjm7rsznqaq8fah5cwflmdykautv69gyrcx6lvsr739dh0yldk32gwj6my3ykl3ujvnxh2kpdn3ynqh9fq95zrc2ykrmmu0m44sj4mwpdxcvaln9yfylfekp3fpugs6p5zjs6g98q27qyd62lvk9aksylx2ap95qt0ff8uavn0zs6mud0xj5c8fn9tp4mt8092nnqxfe5yv6e5ntx7cj2vls06qwjn4th902ys5v78l6mau8yfpwvwswqvzl40pmm7e8cmd58vv3pm52evwu7ne9zc008suv797ddpa5rr0avl6ueypzkk4lgc88drgcgsdzatz00j8t9mygqd9n8qa7d8ejkwhxhhmt5lhq3jy64qw07fa3lrmr9gs758wlwe8n98qjg6ycyfl65vtnmdxy0345f0lsce8ks56v4a0eewwqsutk7dawvywh5xr99essjuv6dvam72q4gzh7rk8js7tdxcq5as7ml3z9thmz9ef9q33kckv6ln8e7ww3r5py3jm4f3yundccgu4d9aulq7z80au0qxj756k77gz5zj9rzkutcn3gw9d66tgekyqt8z604csu2ny0majz42zndsrhrwss3n7krk3qyv6arjd6wvqjfx7a3zqph6002ggywmyae3czt56g3tfeuujkzxu955p7urd7pjg53xn7m7m6m2z3tpm7nk6d4gusmjs5sen0uz4tn3t0hxmafvgayhr64xjuy2cws9lj3c94u9gt4hmm38lprazn822hsjg0rzgdzpp43nc5xj9t74qyu3cdrz8p3ndj0j96rq0ggmr24ahhuszvh7rjfwuwvd3uuxt40hflw0zgz0z0zq2js3h4cka3wcwh333a78xuzvgjq4jpvxmgww2ysxgvpczrv86w0q0rqsfud0xjr6d7wgeddqvtw3lz6eh3hqvkjea2jgjm4hzcy0c8rq9s3gjfyvknl8ahjqt3vh096ntn8v7zw9flrcxw09hxcuwctux4drk20vtmefyc6lptm5a4ejl960f8xcz2d8z3ka8gjt6yf6k0p7cjfwjvq9pxu5fmq3h98l9gmt7nw5t5zhjrx6rdqnel26yj06z8rxusuct7jfwaydprtnmflytmjzcl03cd3au94g66dmlnen6up9ghqgtrn2xd4755q6yz68qucanamz00rhxhx2ye9lw48a4ltvqp9h4rc4nww6da9rmhdftdeq0h7qxqy99d2hvqn9hthswr8mzwryru5t04nzz2uxfdsh9vw6jfu9xm4jhzvk0p666jmt0cs2jgu229anq3u7442uv6yw7d0dzd8kxfdsgnw8wp4v9rakygxxe5avmqy0d0", Network::Main).unwrap(), NativeCurrencyAmount::coins(4638)),
            (ReceivingAddress::from_bech32m("nolgam19n8e08uvwrhw8kk2h47set404vn2ppyyhgfxh9murduhrwaed85sv2h49t24ap0rn6qtkvhu3y7n3huqln4qv9wa3a59u5zchk2vag8uzk7tn8y9c0s54t5wpm8t44fvlpm935kr6q4kxq6fu8wqf8myqs6x5psmcqwjfhntmdj2d2s2puy5kvad8h702xdzqqa0pqlpvepdex6lepht5dpma25pz4vja8f42vz50m7tusp7fpz263dm5kq7d4wegjgkkjdj7kgz77hl4nsmw5pg8qu6pa0csr7gykmyscypmluqldn4x4mkdsdatlfldt0u89r2akv7tnnlycmpy0kgjzrmph30wyu4g4vyyz0ddkvpvf288y2r87twghvv9264q4zvtz5zg7tkfed55rj6g05yj3fs43hyd7tnq8sf729vf25jjv442dukhdkuy7f3fnk3kx9822elgf8lgd8pvu24gp69nln6m8h2ngserc3ngys3e6s22qp2cw9xjdwr6u6j0a77wsg6z3kjeen64cmjvm0r5ye56nwhwuna047fq3akhz6rgw99rz6udk228lpzn9x8t75l77sm4zensdvqzjj89ft8znhcfqcxzeg2fjdazyld7rfxl43nk5r4r6vtutrj7wxy6qu5cnpt2qs7t4w5r4uxex96vlgp2s60luew9g837ujal7wr7ddws0mj3naaa9egm07y32zk02y4h838c29qy9ru3yltltezz7svqak9sn7y76x4e9wphf8sa0ld485aucc03jsl0c7yrlmlf4gj9gywq8r6gxnnem0gvw6uflwu5ygr70vckpldml4x6c7lfv3y0tll0pzgr8xux3mkvaaah0sv6sjudquudphflw60ymex7xlnc4hkfdfadu29jv4xd2csa3hsmqsnvruhzuctn58hcvvtttpgpf2qc0hlxhu5d2jsfgqexxjtkmttr2c2w78es4uqfdduvpwum7fgnt42x9wx603wpx6gxzxxfr3rglnmgth055vxhvtapj440j0paqncn45u3ktr50e0e8g2qwheak3dt5edgeqqy2e4hhln8puxnqmdg7tadt2ql5ns3lszntky72lt63qe7xu4s0emnscph9v8kxawjqr8znj4sdp3xryvzxj9naqycnaup9przkfr05dynfwzerwfpqxjmzc50qsecd9pk2zg2wnqwp8dgrf5feyfgw3llvgwq0cxw0cyhtuk4mua5e65xud9j0dnd0evuzzm0sk2kwv9al45rrk49jf3wm3gws5zzlq6mc6ll47ul8e88jjcswhj7y50pe2g5w24gdkvxat4nncwc3l4faqykhpnkyzfrm55f4a6w3luz5hr68y0t0h8n4trgnkh3x7hkjgwy8mtt49h8m8xtf95dm5kcnzr9q0v2yl3ww4d58dym4la0ykcf9rsm9mmsuwl8grnntnh9fhxrj3ax3dgnnxnlp7k74jsr6796kscsm3x2dvhmj8qpt37gsf7y382verpyrcferqdq5qa4jmy0835j6yvhmcys8xlw9u32kcfm4ssjd0pcyxntn8uzrs3uhhdz497gf4n2cg94zwlwr6plqtpn2qhmaknm6u7e4dv9exmax76nar2haz3lngj5rq3qsan4r638jz09uq9crvgz333qn2fg38cmt7he5axl2pevlrcw5r4yx7p3kw5geknylcu56m5vm69nttfs99484h9n4ttjqtua4hgulqkv3hzc4mcqtcj6lr7zm46yy9wv7z453z6ph9ef2r8a3vdrcnwyjxqxnp6cdc2xlkymj0j80qffqyr4q25k7tx53cskc9qt29w99vfvprwl38hxhddjnjujqqw0gzlgwum0n64g6ehla3q5d3ate6lvmhdwaertrvm4rfsurvrh3xvry7064907n74xyw4qzvspj8x40fff7aflt9lv0pzqecn8y69wppe4c30wcmcf63889fm5p5p6782wuy37wa02qmsyz4zuzqjsekmah3mgtkqns2u6j2p8wk2qdnxmdmkra90vszkjzum2lqrsp5gfj0vuvtpd9kvey00ckqgkvdzzkrj7r8zkax02tkc6awpjh3fd5ldguaw7ju7tgyzv20up9q6ddydtxm6mwvm94mxysp34xv96djd3gayhxgp2dpuz620qaxytfz6gnd8jdz8vyaycgmpe2svl0mek9ns7h2z93nlmqpk0zrqg95zajp37awcc54rgmp6cnf0vw34l6gtcdlggszaweyuup42pj90hyg6zxff22epdmtee4940jwk2qltgc8gtdljk82ve23fvjkncm62hzuc69266g2j2zxu7m90v2pj034hvnzhvyls59ye2gfs99t404nh26tsz8svr4dt6cpz0p0j2vc26w2upnxegrqwdkn3vgkph5sd23qsc8y6tyuxvcv3l7fq2phuprxry959fgcv6qa7xk37nevyn92yxsgfxvem5cus8tp00pxh77nxj06wwmv347r73sr0gkp294j5xmlnf0nhxxvrakk7gf7j02vpvkyz9k2q5ed6y370tp5kzahm667ar78gdey8wt2ml5jmm553kdn4t7rn76p2xwsj76sdc2zetez2vsuspgev9w9u8v2af6hpznesc5v3cf0h5awk42gda952dp0ytpwz5s7kfz28vmpv8x9mmh4dx65snqt2chr79y6pytjmezsg0dq975mg0hhefylrgmkl6lk2ejfcaaxqzwgfk4v8x3pw02m08d4wtyumnqy5jxsf7z8ydtlapjd53vcxjeslpe063tcpww9g4y4fydne0kuds2ge28y5wjejlvah4dcc89vtwtf4gv5ue8cmwxqyfhfuhjnztckws785j4d4gnjk5ef25lf5yxfazt47t0q32wte777tusttes77y2g8vpayqjn7zutzk7ufcyvudxkm90v35lzdacn2t0m04nzacg37rcflr693evxrx8cw3r7tyq5ef4pxpfdfqwpgz0we63r6l6r2ekjpu0vrhae8lhayr9ayw4d9pgjcutgzjsxza0aauwmc2u7eckhglaq7ngvnzv2s7u5p8ugd02hg47cwncqssv4f2z0jxgeyh749fq4lcfpclhw7urxjlvm343jgxtxqfss9kwpcadr8xtfl6fuag6zh420ak2kr72fp34xk6lpwl6ft8rl357jw9lngwk2mu4zj2mzkv6ewj8jpjnghkfsfrnndwh7urd2gryk22hqqztlrl3yz5gefumuaktrlzed3lrnrez4hnkcwvthgd3t2x02js6jyuzuw58ap93n4p5qrmprw3r988q7hcjhnrhef9d287cgz4ltxsaltyqtvv8tuytmqtv5xxq9muz20r0xqnzjwlqkqtf7grvutq73cg3p22et6slumx2d9wfzskw8", Network::Main).unwrap(), NativeCurrencyAmount::coins(234)),
            (ReceivingAddress::from_bech32m("nolgam13l3ejmpjkdlhcacsmuent8yun3kxx459xy76hhzxufl4h3lm293nwl0m3cqrtqrdw25uxvkqstuzz983qrsc36235s9ajxlwzljy339fy5we3s22u709y23werncd90ead9vpd2c7qtkaxcq9zs6222c4w7tgtse26nr32vqcd60jh3pwd2k46wqgjv0txqgvdnfh7s46k00v3tjf5krt6zcqmtl52ata38jnv0r6eqz9ukc20wntmf9hvhpjve5xtwph67wfsjakecp95qadfyp38qf2qu4pfpym82wffa444ehf74kwcw3daltyntr4xt8cqys4p7mgwhgr9fz205xdqnwan3py4786jlkrw9utfxk2pczvasswcqw8n7crv303gf4z4az54ghfjxwukfu26dtlj0feprrclv8yez9h4460z7ejy7gkx3xwj2cf2k5kug3vysh57mkyzak7f64ujz0jr68nqxxz0ka2s7zwupcmw4tte7kuyhfgxuc4enx564kw489uhavwc0wkjxk9v0l6ezv8llx0ae5qjfjjxru3m776rj3xzkx5ph3cryf3jsq2lglr0ymz64knp4tx2agf70ednquptvg67tcawum693e53c0xa6463m9frt7nj3hkv9x8nu6fnaufk6vnvhqfn33nx6ng0vg35t4t80kz0wt5w33nrwgq7g9pm0v5kwy7v5ayhse2456pnu80r2tecjqgjjkqey2je2umf0sj9qewfgv7zdjhk3q0z6prektgunqp8a86tjjathg6wa3snmntrt7ftl2gt2zhwvuwvma4zk6z8lc2l7aezrljq3evsc549xudsklz22pqs064aw80cj8ltu26y9ntvrxypun0eatemfx3wy0vg07d4unfjctye94tlr9p6cc7da3q628xv3hshcc2z59qudvw6lhgpnazeszrp2dht654xw000dpdkcff3ek5kx8t4evf26ahpqhmjtyd236ffq5jnc7xdu5q293wxrzzqlq6mldg3jdg0zqqwtum4ym4kp39h4z8ztxsym5s78e26dtd9egjtrsxm7rw6yr3xr802zrd7uan50salmumusra8e05fy5ruap95jgkd7hgycp3fzkcq2g4e792jagvauz7ct369cx2yhvxeyga8v0k8tf5meg6mwpy2samt23fnlz67lk903hwnjj5shr57zy670qmkxevk8nm7t8l7k3suq2aach5cetq49xkldx3tgyfjy0w7qlsrmygf9qlw78hka8amglqsa7jk06we5rphj9075pw899ntdqwv4lr9j6g89ae9t3ru7h8zccmh2wv8z7pf9vx9cy76lftcnrspz4qdqukjzrj7c09psu0er62f04y4z4yazeazr4k3chvf003jxk9mmf9s02asgqxrc4r9x3pncfkdyd36uk357j4lv740yzmesl037purzx9tx0rk686lf0clnjhd0hvsv56u270azcu4xumqxzq9xralarhx35q7f7gmwp49ztz8c2c6n6fp0zju4tpz5wr2qvasrhhjmq483kl88gjfdd06jcxw5km4tfsrwmg3r9hq6fs43upntkxt4yggd0vz4vv24jq2xa2wvrz6q9k6drfwa4swnhs8w54wp84e9r5yu03xltzytrza4xk243tq7gvqt5p3lv3w4p0asn07vwar7wzkmqk5g5c9ry4hdyhmm60cdsajnk49lz0zncusgpcxfnzeh505lsasvnwegtsyzdgp6gf4c70cztgngv9l3yn3r08lhazlj40044jc793umd5huwsfxd7h8rnkpwhlqe3xgu9p6kew0n0np4j0s8fwq7lkavz65fdafhr0uu4tx2sx0mthasnmv0ahvu46n92vxnkj54z9p2pjflxckl8szykucqkg6rr5z6vl7jpeeqd26r4xkaf85qvkwhjnvzmp9q9g5rvt8rfsmdvpta4jyqdhd7nzh6573kxpqwxvnnuj92228xtfrtn5pjudat4n7amcrckphpap6gffnzu0zd4car3p0lqfez59wd60f99exp7q35txx3am8pdaxlqtpgyhmqcuyfdnh5c2lfpekd09r3886xxj57zrff8yly9mzpyhuru3vetxagaq44ezs7nea2xn4qc86ns5d4jelync52t5pe2ucvnwqpuszuvxc4r7gx4x9sqqjr0m27qv3dxt4fuyfk9h4p7rxzkpu6u4hp8k9xc3cz8r7lntvz4sjucwer2j36tu2037zdksvdwxyj26k0g0c5tuqmngazapppv6reprjs35e8dh30gxrw00e6sm55rezre4qf5082aps8m7sfz06avgld4p6csj2ehl5a69mykf3yj5sm47faetwr2ndkywpxlgue6qaj504yw83a5hccjq0cpam53v393e9rjjt2ta5p5t7fqrvujqm7jy9tkjds5rd2ykmumaktfk24ypn6ymumeaeyz852srg45yvmzx26m428zsg5uus99kvhsm6sh5wz9da539zpy74n4p2rgez6qt4suadje28s9f08atntwtrpghn8czsvmy5t264n3zsnv02e20ruxlj3r89flmvg4zjtcfe5shwfhxjjzw6e00qyny67h9faz5adrtkmcxhz307xv7l603rt4wn5esrne5uuhdt62r3yqv568e7fhhzkgkc5mc60ds4ahhu0gkk9yhcnf8pgjnykgky3jyze264nauzqewwhfgmyygess32zfnjwn2l7w3qfu6ujt47s27lzzn4sfvvlv67kqka5zc5g5tp24mtqcg50exxlkzwm4s4vw60xxd8hxk8dfp0aelrhmd9sr54fkjuclu5shqhda4332mxakqe2wvnqfu25k23r60kpdw0k9ht3fqflu09c0pe4xr64qdfzyzw0wgqua7sv77jrqvgd2lz4w340hhkx9086xrkfrmka0pprjhp58pldewge8x9qncm92h7svtc3gz8ds93agq8fx028fc8hcczenre0dl9q2q9he36g9xlp39ycjsz0kxmvdzpf7s0pn24gll2sn0cjhqp9xy8psswv7kp434fqkhkkafe6eyvgvckjs0sxwr6n2zt0u4pwel8zw7hkuw8dq7w25qj8s6fpyrdyglsdr333a3xtq5au3lj4appug0r0z9tuvfrp8hljanl5k3rcdtkmcz6ypt9g4p86xfke67ravhf63axp37j7fmh34rpc534perz0h6rlhz3v36q4fc5788zctcx3zfk9qeasmh7akyqrme28t4e4y9cv3wu67vpuk2qs0vwftn296vg7y0ltnm7pvstvepk83varxcj9weu8567z0pedaml5qkknzqu5ea0ftzepygxqmvcaq85394c5q6044ekfp8vkxw9cp9ehj96hq73t9cp9neqnhmvpshgxyxy", Network::Main).unwrap(), NativeCurrencyAmount::coins(8315)),
            (ReceivingAddress::from_bech32m("nolgam197nu22950ps8q30knezwg97pqlaxuphpv5fhdl3ms4d2cpt22n4x6xzghefhqjrjr0qa3lnpptsmmryj78zdkpc3q68mzc830wmdswlpnkmcqdshgyx7e4g3u0cqn6e524xqm4eaa6fst7pzef8pv0mtarw26nvxqmgpc65ndaqjwh2lqx6yj4c8qphjswgpxxk04jyup8wz5sfc072wf9c2mpv6hlp5uuewynudyrvq07vk2w6t2r7ycekn8eypp74amwvqhp07648tkmaekz20yr6595x48enr5g7j5u37u9k8a29nv9kyspsslzhmkrlzfs985z2g3vunrrpjr8242u8su0p57qpw0hfzfkhtfsjnw2a2aa4rccjzmk4rr200v4kvd5nemrw3fny4k2eele3vk2a4x3ax9vmasngzh0aak45qqq5gt6wzc8lxfu0k2t8u357tfp8tr3j4kau6afagch92zdkd4ekflhsjwp9q4y5caq2yl2hn0gqsfl50qruase85c63vy565lx707yklhca5amz56hrag30ew4myyfcqvsfq2qzdpfyprrld2fg39vckvzpu2hhfqkrxn5khpzc7lxnn2lk9jyywm4gsa6lukgdm6fv8l4haez7swcxgsamy67a9da8yy4hzuhj54vkq5nl2szgjkuk4gl4uyywv37p456k2y70x5mcyjpr5t4gkr3n6dh8j0p8kqyskx8uct92ke6kkn53fmm4ddnqzzazy0rs6ug70uvfk6lmtyadkljqjleg3cjmjdq2q0lf9cy2j2k5f8uksehpr5ummaeh7dvrc8wjluajvk7davns0v09j624um4y7u6vw305m4xy5k68tdhw0gccqk6aadtltyfyhuj58ykqdp6dpryxrlznkskz54v7q58rfqfm4wj7hlsuhlc3almzqp8rr5g5eh9cqtqvd6f20vynqtj4lr5evxazuvrk8hamk7pxnnc8quk3usrhltkcys0jrjvuqa27uwmz88u7laufkz007gunj2h6gpf76fyj4q8tkjpzamq7z504vyht3uqg95w5ueuv5yw2nl7rcm63hj95jqz88g6cqa8x7d3tnrva5car89xsq0qazv5ne8pfuvqzaqhzeyvr06h2slwza98fs4qklvyr7dvzgjwgj30sfe2gr9rsq86n7ae7cl49fyqfpz0wagveldw5m7pmn2qg6hz20nljafgd79y8v8ww2hgx32s4nxha92xt9zym00fq0qgjrc36z039yc3dhs4tzrguatsvhrtq2daw6zp08yyvepgv2290tadq7ezc4h7p8l99htpvn6at2rclrzj0fl9avy40vyjynwj7yl0zja6y6s759tyvgzkmvw9nuetev887ccel3gzyayr0g4hfv0jyk6stcf38hgjp36ys29hwg5vnpdy4gncz9e6ta9k0eff38hyfn37ehjl0d7wt45k8q5a0scfd69ks5fcg6lmz9tku6fmlwqcywwu358lmycqwparc3q756nwurxcdq5r9me6pmfu5y0u8vmhmgcqw92np9zrgyqv6dwadglu8ztwr9f2q5trran09gjxtnkwxtymeqanlxhagvtm6acmv2qte7tad6ws7d99z32jsjlp29mytmn3f7hh6247uhtf20fj5gja8jw06kt6kjcgv038v8atdqq5nk96trut0zj3a07ym2euhk5wqld4nj58ddf7ljqeqe770d6075608k78h0whea8lffvqvu2fmug2xnj9ggrt5vygy8ltqxksat4mz7lrg8dtlp9uakrjxah9h2udcsw7slu8vnak5umamq0mrqk2uhgn0m4fnysqnljl7cygfpacper0w887htu2jhurp8f679eyam7895g2gazzzhzeld0ll7z9zr8yszn33fhrr8gc8ve0kk0vzaxk7wsu2y0gwyj0m8ytmzrvfspfd87j3n9ykyr96xs55rsy4xzjyfnqx66ch2vn8rj2cnn5hf4vhs0vy8wyxt2jcq2qwzyf2txq2e25ms0d4s0565mne8nl5srx7grhuvs5pfhaar7kqpvtdt03huxa98vdex9vhrj7fqatkq354y346ynnja68nwqyg7mwy44tpfy7ld2x78vfy0z7wgntt5qjrfva2xjzzpjj8upm5f7uq59mw4cucj7jsq8nxxa80gvsj43s8z79r9aznzhxhkq9509w2ueruxkyyvrfcvsusqqxvejp9x503g8zap7e2qqc7zkllwegc8a9peacxxukcle6et87uzmyus9fyejg77a04xhcsptx7cp95e3gcndk8mz9g8jc5k848f67ev8g3ltpvgxzfl804kqamqul90qr79cxf5ywpwfk9utdhpa4g3qqk0anvmd0as5uqrn6sj4uup348ax3p7dvz5wd3qxa0d7g8r2me9kqhhf7rtuxu5nkxd6h0u2976jp79umrldf9lpdnffnrvsdu63zl62hklyv9fm4ajusekyvepy7jkfyhykw3g4dzshvhs0wnnmng3h68f9ds3njeens0002zdhzjc7rn5h2498uvvurf0lkz9kyj3d5pgw9st3sg7f707yy69uujlhghjwcz240652mdahshqfn9w7kzlsypg2zpfxzsku0a54325x7xvt8n86clwnw79dgh2g0n94y240qh2zu9xaj3l9ac7kh9x4vuqx0lrxs5v2kzpdwg7cjfwljc0n5v6gxwrqnn3hmxl823dd2fuaa2j6dqzz7x6hcwx5z5u5hwhl6mhw58xu9kdm8v3u7klu3azvshy5ep9up02v55ew6kncs5xvexwuyleeh6tp8ucgvqs0cz8c0y5679qa7y5atvter7hu00jgm6j78qdq7kqqst5v6s784pdy8e8d8ncjttpn33x5plq4c69p4rwudkx4sdnsq0ycunzzxwv4x5d27lkmmc3662le5dywdg487ja366p2yp9tu7pr8xrhlr8uch45ruxyn7gpk6uzsz7z4sre76gwez647u4srtde2unsmxtc54tcjs5nm92unhp6gjpam2e4hv9m32xpfxwhs43zdgshzjgqvk3u5d4tl2u93w3la5yyqhhvpat5p2l98s36mmgkhfqd0rmkzwdx7ctdf3vvxarvza89qt5n0k65tgmef09sf3tsn4v8lr228da03u3rhak5m886z856y5r39g9zm90lmtn7ehfj2y59syqeykv29a8zcf2pjvm5v85plp094eccp5ejrlemufjpq8wzlr9t9hf95f3455x7nns73dy2rh338tnnfrm5kk648qx3y5ysaggyuxrc3d0ukfvy5vmxshahfcp0nfny86hwjruw47d90gjxegf2zf94pv2fpu824vu4kzujdsuwl8jnsnl07j4l5tcmjwaxd8mafrllezfz8fty8d5sld", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1k8yv5xcdylzt4cve2zy9xlvue45l9pdm29zwnjgvm7yawt7rwa6rg8h9hwsyvhcpt7wgwfjmcfuyxmu2lfag26x2uysgymn80qfe6wl69x9hj826cqxl89lnffwmurk8vgne2lfjp87xcdy0ypannvm7j995sjrcentenywj7zplqhatealmz9sxfyeue7cdyvndyru4msg2atatmg7a7uva9atntxvlmg4jy2z9glz0wk6rl6kss42yl9tmez39x84nfmx6pprywkn5l3g7fs3x3a5p088kcwpx7nu7z09k792rctzt8zjznk9vymslpu5p4967jefefd5vvxqudlyhf38hdnr30jvdrkzxpu0ns5dmjgkk5cjk0lpvd8cmyrx0wrc2e2nfg3vkgeft6cfq2ccmx4fkamq8k7lskpv95qamqqatkqfvd8lpzmylfvl6r3ryvymqph243mw487l3epxux0lx7qkwf49jgxk7gt5yc9dcq8wfrw3f0w29lw5kc32d2znjfjg3eu7t63s49gn78p8jnudp6yynqhglekctvs2sg5drekd467kzdvsynrg6djf83vqjwel0qndz2guk4udv8g9fgfq4mzc37hyukpuhd6p33ls26wgypl8lx07uvcv2r0px2azl3ljpqzv7ufujjaf97cv2gy00lukfm59xxyg6exe8nptcgmg4z7mkj08e4w2ayhhf27dn853vfd8aegwap05h24zp3uvevx7fmxzkqug8j725ym4nxks5733yww2yz0m2le78vtxmyt85w6gxt639myv439s8acahc584q4z3xp3xw2uh9qetyfaxpnu5en2ac8zadfgqfjl97muw49f7egezyr6ndcj6v6mrg5zmc39j874648e5tghaaqanl099v54sca0ecqyd898qusdjyq8t2cl7u4a0uvfaut4u3r3n5vuuftxsj6cw5ptahr2pwxkxs3l3gwldpmn09tzkw2awu5j7wd7r7r5np0nj59kmv6py2z0txne0eh9ala05l2lfcmq7ge7dd6jjuhzew66mets7u2utxj399k6equgtdv9hfs3f32zytepuaj6ggswxazflrjvsrd5eug0smrpxvht77y3p6qvc25y8duza2cdh8mqkyywavm6rvnurcpywaxjmrceumz5pf8ug9twkd35vhhsd4lynprspuf0cqpy5tj2fxzsx4k8adz0x2hq9c07ya7mnyher04ex4y3j3u09j0umchy5vcp7a37e7yj8zcelw076w03pnqz2v4emz5zze7lhuv5cqgkxfhkjdgvgmr64taenjf0kx3gtc2n8j6u45u7pvln8hk5fftcc45f257553vkhtex90ae6fzk4y26ywrqe85649f2ctljn0rwdz9csnz4ga8yu7rgeqhyxtqfukvvkz0nh89g8dvenuqlwltm28zd2a6rzkjwhx2tyhtkq3tmn6kzvte8sjwgu9dt4ul809jx0nwvy4fvtz6unwa7dy9hm73u6rgahx8gtu765pkcz4nwsmr600nz4cwjj77a8p9v3c3qzy5glyy428cgngtkqjpf3unykakzx0w44wradx992d2yvyx5njudmtcsmuujtyhktgev3zyx3kz6ys2pla3j4pswqqe5rqp82gr3avmzwk79lycyllksc5wa92f496uyy0fx06enrfz4m59ls2e6v5v3hse9cn4lft24yedhjcddxmlssllq0sp2gjagjpvclsveffplv2fa7l0y80mdka6768qrxj9rzapqpfqdpn07lpa925d3lmhcjphmy3pf4mulja8wm8kka4uwv54mzg0c8s662gngvwvc9p9rp4zvyq67ecmu3lfrjsvs4fn7yfhelu5juzsvs7tfpjtym2pahkl5cmz7pqa2th4ztfcql46wnnukqkfafdymt7dkyerk75yz088vc60rhfthp5lldjwkys79rf7h2tm3rr3pf4q4awxxxzfpamjexyfsrssyjdle4apn2q7cdhjvsghgl6rrpjaz35tdrn8rdtthrksru0n3flf7czchv5czfgtr8jqge7c3uj5aa2c8an2xrvsj5r2h4gp23660wdg3gxjs5mln2ftjh9r3qh2u668gct45kdd8v0sfqnh2c6psgtalxhqp8wm0ycn7064nqdv84yftnjy8564mjz7g5p7k7x920vdrv2tqu40w50ut9elmurw4gjdyzyfchnpyjhmp5lmukuu80ralwwmk5czqfhg3pxtu867mugpxrv7gnqv7vg2ukegsfk230vsv4cztn9dh3rs0yanw3uphh9307qg5qqgw2ynq4nmh7xwwe5sgmqay34453hmw7r3alvuan8nnmnu2kqeck7f3472lqd09dlfnn7slu3mgc04h4mrjfm3saex2symdd0r5pz45hp9agx7amt9tuyjwslyanfwp5476cafsag8yc07d7f55xhe8sperslxy8f3pvu2ltt9tgfrgvqet5dyaj2v4jdtrck7mp67urvdqawyvzucd9ygnvma00zcpj8pf0q0ecsdlzce9tpuk6eek32sltz5jkkdexqhcnrlajqru8qzgzu5g74hqmuesutdp8al3td4mep7qgfyw9wdfjzmgsen3fanud6962t32cn9esc7vctt56ehv8rckgnv8th2gctr5p4n6j9a0x8pv29zv907velpywlysym52pwh7hjm3a7t8ts4p58uj0ehgxzfxqtf7ft2h2u57jumv8wd2qnewmqh4rq9lpz70rmeyk786pe2xrj24tdq4u835lk2xt3ngadjcex8dklvhucycdmag3lhcdj6twgpernv39j04nrq06w3qms0fgcrlszyqh0ejuzt7cqa9epsakvy5p2577a45dqa34se3qce3t6uun34s6vqd5tcj8m37vqu6h6jtu9c0w66q9dwjf0v7pz8ve3k4fm5wcm2wqcxl0mykr2ed2ryn8rjhq4273ts0j7a9d8mze876w0srz3558fw34z75xekdzpugm7pkxksaqrp6ere64vxhyly8u6lkjmdqgf3haa0mmu7yxrll6lhdvv6hzzx60mjtqe4w3a0crasmpah22uxz5hpn26k0n57suevflf7945uggx687jktt0cfu8cjnqusae5rh7x7ge98tyuqsuy7pqnf8vmlaxnagjr4gc0pg00qx3f5us8qp69ym858c9g5aauc058zryna9v9txel7neufe86p6q7vyvkyj8w8fu8way3dsptz0y7emj00tnqme902fudlksutq246a9w3ya3frwr905pnk2rujyu69yede03w93zuhg2fl5wfzhs9hgrnflhlzc3ezuuw5tewh2uxzzgt8a3ylk4f2r6jjhgcsfttdq64xvmwu8aw3ue0at6ead0spvlppxlp0wq8", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1tr20ahwvttxa7te34tkl7dlalhk3wycfwcc93rte6jtnaw3kln80cmjj9zpdmzasxllg9qaqz3vtxwnhq6syyh8q0n3cpmxgz0yzu0g7njcky0fz0eetq5ldsnupyx6fg6m5jwx344srenh5ea74edgx53vqv8wakk5cgcm77fzr8hg6zpdwrxdahhx67ewge5nd4e3puk0vmxc30wv62x6h345murrd4s0h80yhp0xegrq4nayerpcsr9deucq6lwjc9tgazfncrsd6yta7vvcdqx6hs3j08uduxda5mn05w7mg9yycvj7c5usyv4lpmjrq2khxdj3ye9u2pjkjjygmvqcsszfwgea63mp5g3lfzqa5qda4uvhyqy802s2w8vxq6wr42nqrz7wj4lsc64r7g80jdkfh5jvhk42yw6tvpcv7kdrczl47yfd3tey3z9w3984fgner9lxrmnftyl2x7d33fmtcck8wpaa9gqtypsd4fpcfymzgfmy9z30t6wsrgnr5zgljxmnsynh6c7mgklfm67xg0fexz63vtzjx9247rrvf4gxmyh4jv2sxdldcalvqmnc9nmqgh2xl5ereum0t0rru4n2rj8wskpxqnyupvfr4ptwh20c7h3rzyvs9ggr9y6ltqk9dlvsuuuj6ryup7wxvnvhj8x7y0yu78f5d4t89mqyc03vqkj8l0jggz4w5j4mfgglu75jgl5rdxqwmrq68qh64yhkk8cwkqfeyj0vuppl54zqxnuq4sxkm7mv6y6sxwge7py5pqgha8kd9tfdmmzkqsswfzm6g7seejfqgcnzctsj332jjrze7xjy0rxsf9undt7xn8r9yn0y4qg02tmu0nzay5z5uhnzkfygegl48xl0nnegp80wz7qy37het9l9r55k6rj8j5fpdjjf6mfshzy2ypce5rdk32c5g5d04dmcmchnchgpm34023rfhlmdpdktsfymg6w5vtvkn8jsghmhymclss80e37f985rmcglqk4zd5dmjymnqkjje8p622usaxq56edjdevz0rzth9wkjlttl924e3vvg5yr6569zt3umzd3qap6vspr3fae9yxuhpahr32mtwcccpj22mdcskavu90tvnwlzjcwfl460x4rt3xuej9vnhwc45hvntnjna2rw9gwuqyr58up3mmyll5fwamzue6zr6h9drq7qpm9n9hzgvdxwgnwtuwjmgl64r5ecm25jhfn96f4qvz5pca7e93wujg64p7tmw7z80g5sg2jrvhvvuv2amnwjvynzcnw6nljy2htepwpnvzjfxtmgkqxr72tmmpq6ynuy9egd8n3jkeztu4w9q5lmudph2xevwcfpxcm095a4aajwd9k0c4mg39r7emhqhpe9mfvcsjzg56q0laxnuj7qm7j9yclchtxs5wqeqs5lnccpyh4fmkksgxhl2ez2t4jtru2nqkhdsz85w0jcpg2mr6w387g7aczw7w0gar6vm4x32d4pdkc6y65x43us8s85mkacp5le4ng2eztmwx6zgkpp7v64mh4nu4lvtg50e7mthv0zxulr7h6f9s7s7eadxnp3cr5e36j4xugx4u6pk33x6dwmqcty9annw95yv5vf0vq9za32evcle6wjyddvepq7ejhselkhztc30y29qpmpg4zm5qtghmsx35nwd45p6dcuppn74ekvz42ewmsffktzlalrfp77gurl6fhgmwumr3pne6w6udqtmmu94pdx4ggtg9d4nmuege387t9fv2uttw2g6xt9qtccukj2fh55gx8cwvxvcpg2eevv0y9ygncm5jydgtqyxrugngvltrmhexu5ce5xyve328tuw0nt0qgzqwfgq544vh6umhfltlygrsgxvtx94sljuq22js8a6hj74k0cuqmuun38vfmyg5tw9qz356u5lsucjmd9w6vummcxd26uypnk2l23pksuuku9g35r4gf6ztg8hg8tsrla3xnl8vs4urcuhfajxxrxaue47l7r7dlv8tzq4r83ulzc448ld3j8na3tg5xl5wfas24qmc8w7thu8nf89ktrkryty95ffd59m044ku4l5cz0h68jl22jmhddjxwpse20tyraq8h20hvc7c5sjwkpnpx2fpljn5hrlhdk97pvthl829ehxfde3x0ud6t6qrchvjvm3kk6r55q5r3rjm7sgrznc5t5r9pdlug2hx27mdys9xpmqsayql3c0xr8xpdnvf39gxhwu2avempd4m50twnw6nmza28cp5w9xmxw0uepqs950rxf8kzjf4lkph5cl75yx85hq30mkp6jmk9qhx3zxe4rayvryf086dsdgmvh0ck2ug06gzf896w0x8q377amaep2vly62z5g0vhsxkhatknlqtcdd89qg78dwmf4uj4u5e0h7hwhnksa8pwg62fu4tqdagqqkaq0mea9n3lac73f87y2wmu87rgu2jkncza7dm0kcx56pqsf7x3nhwx65rrpnta93vu7kzswg38wgpwtcnektspr0xenp7lt833ftpfcw3rtew2p3ue0qlqwxaj5rv3pws80kjv43tvdc6kgpragd4fg3jy7ssszwhfhhq2qp4ea7a4f3sayqdu0tr8hmrdgw0cjyd0vknu09x24xnc6mp50dnfn6x3699rngpy6zxlwmqzcvrn3ssq4wd3u37hq23nkkzz79pfy845wxgyr79009nf28ddxquyf47wujw8jmjssappdxqh2qhs2njq8zkq2yg80rhwqdsjlqzkfvnck59n97tvjpgtl9vq78w9gxvkkhld4mdp8gv5jqxryyje3hd8e8nh42auhcsq5pzmj5u5gh58vvvytgxng4m9jpeu905hw36tdssw6rethtmhu8dpecucecn75a38gr5860ahj3j2s3l70w7yknnuazwtrktnzhxwf6ph5gprvgvkmpl3jrjzum5scp7fjcwhafr9h9s0hdhyn4wfncgc0s3v6jkwcs9ky87j77d2d4rr7fvexm6yje9324fe8v6udat38nn3c5vtfcylhcm2mrgd85ag4syp9qj6fhunttgnhx9earqhjtah8lenhsmzwquksmhklsde30wqlxj3z7c8zmef86g9yc4j2req7n2kwghsk4g28gw2xfp308s0ycvt3a7z4n3a67qnjngppaypwugfjp4thtv6wtculpp8q8rkz3edrlqwv6msy0zlxcs0u3pdstedqzg3nknfkkeklp6qnmshjckkgz8ewn4q36en5w0k6gthsu4wxwgrqrwuc5pq5p9ejvympnyrvdeu53jg84j76lmfgt5246xnknrnh4umeq2vfuymnkr5wyqujrz6mhy6qtp9zd9y2300dp8m2zhe00lp0eyllejafdfqkkwtve3l7jt8g48uh9y8fgzu2769hu85jy6xx0mu", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam136g97lfy0nzu5g0xvd3p4w7fvx67gvhjpm0rl8ewzhyl098w6nk6eyfgczznrd3hqctm42kcmgyud9y6ezpff566xz5fnnmjzmt0rtgzehe23gzrwaaw2vrlhy8wm5tzdrz8u7caf42r8fs5dgdsnhzxp863uwurdrge3hwvu73qj7hftqvdq36z6lv5ujfv7na8t3znzruvxkyl68yruxua84y8pmmvnaflf6y7mx3zdcxp8jw7tny5mp7hefhhm67yvc0dyqshtmdx2pahhuy6s3m90t92p5p7q92cf4re2kazhvatnzusj39x4535ekehqu93nsypv3k8a0at68dzk8s0kyzy0d3dgsarwa92l4tmghd5txhrjx69v09damsfqp9n0xzdg72kt9qxglfh8mtsrsczc56jzcqsaygyldxzr0ceczy28nwysf8jkfy82ge6l236ww6vushfs588mdggrlwphagsppq2tn89ck3pjv6x6sjcvtlvqg9y8wqdm9aatk05knkes3lgcjlzm67xr43nac2jjhx34z2wk9e6t5cd0qvp8kjmy2kf74sthfpnwm4243wxlc6465x5cacghpfny292w7gk28yq7ns5xh0l5qwr0gn2afuuuha2m4a2fceszd2sdfdyukqpcs2urvtu0taap75r9yydwjd9c7q0t85702cze2fl29qhxr7dxyuueudmz5kkzfdkwfudns3mtj4x95ylyzpl5dsavzk0a7r7qtgll20v85du9eeqw3se2utcv3pgm3ct6t0q0c32mvfffmpwzvgczz9r8wm8xt8m792staxjh4rl4cht0wa3hhaysek95j9fmpxz033jktgghwnrlwk0yms7mqs30qh82cx8h46v9t5anlwkvswn6mhvxn8pjeatr4xd3hyt3txhgatvzvftrje2cknauuqd3as87mtsn52d3sv8qn78kfxf7j6lglpv5r8qf4p5gza4ppfdaxl5r3r5qlj60pyrrq9ap2urrqs4wv4wc6mzqnxse4rgt4gcmg99gw2jv3mx5fxl8vh7ap5lxss7lzyjess6hsnsnkd525re3rw8ucpsxcucuf4muw7gckmy2hf5r4u5wz0vz899ke4fpqk5dw0d6gy5vq6y4ayzpln3a66w457p28fj3yx3ekvlpexjk3f8kv6qklukke6pcy9s78z7srr74yp0n3uklyy96nag5jaew6mvlgf67f8qpm329elsand85aq7k7pzs8hyk4xsdg08dpnddmystsnm2jgmf76szapz3urk7r2a2e53ls55wx05xwuztecndqlhgxz03sspl6gkaqzrpn2f9ydm5kh6mwv5tn4fz8qq8e7wp83qpaar8vt8trrs9p262cf04l5vmlzu9mkg9rrwdmqv0cn28urz7ss5w6vukrd5rc7dk226y736zjner6vt6x53kpvf3u2qc6sz66yz5sqfrqcv44a4r4anf73fg86haqc29cvmxvu6ugct27qpatepmlj36sje7w587ykvnwrje39aq8r5myeqnj78pfdpv8nkem3pzywu666p20cp4p7vdswtxr337juv22ghqtr0alvlwneml2kuw2sfa07wn8xyxxy3z8yt5v20flat9mmu04vvsnq7er4ryc5c57lvdn0h6fdh7xu5epeewthsjq4dlqa6djschy28lje7s34dz6yquce5lgpfn3ljaakjplcf72mql7r35kcy9se0hyddr57ss3vm8x9hywrenjxzjzkjk579ar00p3lfmyv2u20j4xrmy4fs2nh55s59866n72hlvmevu2q9y66s0aeu73cj2xqjwg5as2cg2e8nv3t7wfwvhwn3ypcxjk3hpgwpajykl5mtuz6tfpl2hmvv3m33shwckw999r97j0qvcy63pvvpe5rr2c8fk0qqmjxunsehtsvk2nu4gl5674p8u4xxvdl4aqmdmajecxpt6pupf5xpe35nycvnnp4dqkqmshzjs09p2982c8grv22606pgmvhts6run8ppykfkhvwdezyvpdg75he988s8t9zcmqgwpgskejl0erd9f3hup5twkxcpk7dc05ynzl87vfdwwjwvcqy9elg9wc2jzldalxtpkw0rmafmpaxvm5v72va5jh5m8vragme36whdapnnklpkgacusves7k3rf64q9r9p7pqq0qplxrd0cv55n80gv3g08j2jcp0l8qy0jwgexud8amyklk80ajeykqsvnfrah9y0wzxag7s5uaahxzj8yxc5d7rp00vu3hdu5y2f58hetywv0k9pgufqqh0qxeqy6xxzukmdftqzmmz25chg4kvy6r44e22f6q29cm2pyy3fq0ujqhhce20rlnn36mu7lt84eq5uxkas9vjcwhdvkvhls8sle8qu69ky38vpt9stc5d4r8kslu9lqj38lc670dy34amlvmljqswflvvdsxw94zx29dywxx2vtew03x7jsawsef76hw5ytlsgc60nd8y9s933m42tnnkdj6ny74zppry9p4ttwhzxh57ddpzmpvdw9yv44f5wphhq4h6rkfczqpacysqffymuavgn0tfyn8ksd6pz2gl2jalaaq9h4x63zdwxugea2kqlxeuqtvhmhurvdln5ze5qxgvud0mclk54sl9rckytusy2z9m774hplnkyzqhz9auw45tc3pzc6ve5kuvhasnky9ltaqhtycvnrpk7cu7g8xa0c0ee8d43e6zf29x5ylpdvvql98zqhfds7e6fvr3yavj54amguxg0aj35pgzand7hdsre9gwsadfcq89mvwjyc7g4g8cpg723w0tkhsnytzlsdk95w7u62deeflz4m70lg9l5jqkrdurkhr926t6ft7yh8dwcw3a72zrygk9dvdqrw20v7sqr4d7pz9pvt8jxpch7z4tas3p2ds9ksd775ja8wysr8apd8jfa8jpyjp6jfhhezvlcvsg8wzguumrkjg2kuceyqt9mjeanmegqu3k8lkt2fmx25hw7m2yeeurzsc3w3p29haw9a3tzhehm4ppxf9hzvxza4kf7tnq5e5hvseydahmh0gp9exe74y4e8nerldfpdcwr0pr69vcu0d4ptr2hlsm0ykzzwkqqv74r8k75gsk5wpg88un48925ue6f5qjwtxj8dpeyaw7kzu6cywvngcggf9tv26cdvh78yvt5qqvycu0kgnlq3z234t5s07xg3r5jtzeexu6za0qlju4f8mzhngk9k3qvu3xdknkynw2u5jd75kw5902770na3dxt9du2ma3623wd3e4yaw5he795zclv8ntv4409z03mv3zs4w94neq4a573cd4qg03rz6w4vw3ms6etku4a03dm7g66vyyqeaknyeukelwl3pap5e7xzyra5dm6ppc3hmusglg6sevq26g", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1082uqzjkng5zds50ryxy9g04n7ghmgsstpxvwn5ez65y6qxudd2pr0r7s0k7sfujsg2hjf4nm9dpwvqygm9xp2kvrgprejr9zdm08tzjqj0dvfxny4jqchv3xzxtqdemgtex3z452s55k4twuxn9spqg57vhlh75t5am7cv8llh3usr3e2yrm020f02xt7ushk58e6neakg0r4qx4fers20hj7ktdsh46nt82qavf8mstpqw26zmw4c2yghd89n0vxd63xe8xlm3g3u2s2srllyq3ya08k6729vnljw637j538tjd9nc27n4ulyvrh0dtehf95pp6a9jprugthy5wl3cks3ldgcmttum2pmkqxmh5aqlpuajaap9380ms7746w6gvfhmy839ushq6tmhhm5t6ppr8sdl5xe2x2np9n8ynmxq2v2ewa2ylszwkuak2rkufv2aqr38a9p7ehg093u9d9adw8537half47gywvr7v7e5zm8yl4x4wncyftz6chp7devxzx74lxtlxt75x36qy8tl6yxlmen66e87xf2fpqdkmh4uqurxtq2swnrzgpd3mesx6plp80xkszkg87795hrlmer5a7myjv0xut5vp7serz8m0mc3dtw8hzw5gh6ss2xnkmeehd77qzjtzy4839y03zvh4a2qu2ez36lfgkn0ywp69vpch4p9f7zupns863ye5t4xgrpzhzsypspdqcs2qw7eqk6gfu4u74ls3asfepu78qs3247a45yzuuqeeru6fk6xyl7tuep8e3tw0uere3mszav8r8n60haj25cs62lwc9kaxfgmqj9ucwdyswx0v24eljqcg2sw728x5dje5k3qcme0wzpkfyy0sj4e46pnrjul8nqaqlul75p8y4rjf54zckxe4lzknegv6vqze5qzvxtl4gesqsd5dah8q2veewruu7mpupv29wx9ea3mxv7e0vtkewl9l6uz4uc38szzx2spya07h8rl5hqdeeczgf33fh6tg5jdl2re6398uwvccjgxh47gzpg59tfz00s6nf8dr0mz0v2kz7n5tnxvk4wr42we8m4r4xa3t9cx55e5e4pkcan85qeyrcsdlcgpanc2fy0aj3x6jkz74845a4fhfq6phlktl3ayzltwujzwttc48589ekfspyj86r5wejuct5rtq7gl7qgtxwh7n374dtzmfq0uk98nqhrflrsal5g4pytsccfg3qd78v4yqv2h3zvy8v0k2qa3l2u4g5zujacn92k02tgdtunz070h4rxzc930n8uk6dgzl2scmnn0ya75zeaktzk22437mr8vz5a0g69gtdx4qvk5tkxfmjl3vnex2n8dfw32p8sgth624pn2uzrsd254sln79ecwruk4qyv4z6ku8w7vj25jta04zlfnewsk97plzyh2sgqn0zfyns3u3n5z4h3ehakg29rsz5a9x6rktkm547tvjfjgnmdax6027u07k6hraxgtk880txdpkyhpj8z280ekz0l9nj6dzg77uakusptfc0499rclnpt37sdf9p2cf8g472qwjy4xe2vltj7csjke3u5336f5h4e9kcesxt082prhqs0px888p0k3d6nqaz3h75jg2kj4c80eztnspx0ngly7d9ztknhu2wqatrdcdpscnycu0uu59062mhhfr2gsvcdsqfxlaj4m7sddqh8m34n3mpv0wyfnwa0rfdcnunh0fxqmupjgq5rs6w3agxztrfxews0qhz0cjeg077ua7c0yq43w89smxep37ghdwx3dyksplrx236p7jcxgua47jzv5g3c69zx63f4y8ncy39mv3mednvsk62g5m6a7ue0rtdgspu7vg6nv099e0h3u45epatdmkd3qr6vr7es4vjgks9550nlr65uxpmkcjqvw3pyut2mqda8sfruke8cswgqqalj45s8qxla35hu7wzlln2xdqyx4gctv2epsxjh28pmdrrzyckwfr4c0nz744dl7xg7qa8dfehfg7458fxmjulqlyae9mn9mkfzsv8twyexlr3eanfqvgs77nu5v35yh4pf68zr2te22u80k9uz6qq27tf2lsuqm4uqxerz4n0axpes8ytee95gfqcvsyme80zj2s45pjpyng0w5l7wd8x89ejn34q2x0y3ayanr0y4cyyyf5ldvrr0zat85nvgk3nkzl4s4n8qclsl0h6la8s0w76kl5ppmsrqkr28f3xgmvylq3racwv6wf2w8g8s88uqwgqzc8gh8h304nxdmj688mz6kd4ftd068d3qu7wycguq0a98cwwnp5cwv5s8uger34ay2jka9mr722qvhyv3s3q6vayaepnrtdk25jmfj3llcwm69a8hetujjw62keudkv76zmy3pgpt4wfsc7ws8424jue0km47et9s225f6xd3fzxcp4haqnsn0p2jtmcygms6z6vz7tjjfvngeuy99gphrlv8p88tjfd83l0yup73unwpy5q80x5msscy350tu9se2ye0vcvtejpgy2rayvcc6tsld0js3ndgf2mus46k9h00muhgyp33sjn0n2unxcn8cxfmrgrfhktvnkum79ndn9zh2s8mhhc3jegw6efjumpalamtv7jyj73zckpua9sphecmqkxxnzxkfkldjawf4jnwlj3mh67vq332yxae5xmqy24yuqjy4u6evcfsha8he56005k4hp5s0hr2en6xvsvfm6j7xuhqdv83x8rj05hga453c8nvlfpe0n4kg5khfk7rary8dz2rd2eas034q5c4gv7gkqgj7prtlsa6ut0vuqjnrp6ktrfmk8r7crpg70xh2r672qcgh8vd70tn40mcm8ze9lrr0jahvvjap4l3qruse46zaye9e6k25xu3ne9z65z3mst403pcm5r0fevvdecf2ana4k5jx2s3h5gtknv23v42zu585drg4ewxfk0qtxf7npnhkfzcwhc8c8gurd9a4l40s5et8yy35jw3mlr59ew5n07xftpacd3fcul6hcrd2jafak34pq555wmtvgstd9gckas4g9negwgasgkk4fa5htvnd878tkah2njrh2uqzxts7skxfw08t6yf87t9ryltnls9csum0ydm3lwn3rjuw3097esyjcz7n3hpkx32h5yvsje9h20emvghnl80y83ylkaq885m4m2y2ee7sc4fpgkmzwlkevj68z468fv5kc885th7qr2kv36uty7gp5l5777h95r8hwqcwjmnnd3yuls7d5hg7e4vs6jqs0x4wvmjyghv4gwhwntdggusn6zl055j5y458eytlyuk476jc63ysumds9tatr20hhdhvxg6lrvme9nk5jyk2ge5j6rjv9a4fyvjf77z07gcscyanuduymuyuysgj23symgamwquzxcj43m3p9e9dpnq2pr8nps", Network::Main).unwrap(), NativeCurrencyAmount::coins(500)),
            (ReceivingAddress::from_bech32m("nolgam1xjwer4jpvwxwkwegc9uqauqsj67ctgdpywz5yukkg9wj6gygzq56wpn0eehhackraz5ug6n7rmr2ts8km7utdl5wqdk82x3nvss7rhuj2rgsjx5d9n5y8l36x7wjjqwxd8aah5tvj75g36qyg9w4zwqghrda7z7mrjzlqlumu4246jkc2pem6tcwsqvmqn8kpn66junar9vgsntzqtdh3wvt63yf2p4ls6g4jhejjcxl96jv66j9qmmdcdr3jn0dde0yump2mjcj4gyktx8nrvrqr6zr747j9989q85plxtanfda8kv2w8lape7tejl3xs7alzaacys5v2duxyftwuwcrl5nk7wyayc6szg6wfvz3u965ttzwp753gvwwsyxxe89v0se9j0p3nzkq78y73wvy90hhu22ll76gxv7pe65md6lmnsrsura30kw4aqn7dd3ycewsshdysc534w7cj0yrmmq6vr54ja3zwy4favhl9jy670kjmmghlhk848d45q0n67fj82ynx0xlf3764jl93u366nlpmmgjfvaclul2ef46hdvfpt5s42r7mnr43vgr6tcyp54nhv6mx8kn3mzuzv2fgeumvscd6suqfvqzs36p3qaewsqwhascejttyfsrpu0qwvm9kcehkvhlkskz9def7phqwclswtgyggfa8kttpmh099z948fhqvp9jznqm32qj5r07jnz38hxq4k3qy9cuu7259s3l4ujetg7u0tvctnvmhnlvt4f390wqqgzwzg4pdhlln68a2j500n89hhchrgefyllpetlss0ggw77xlygnrqmufwgk0t0hupcd0m5xthpahvmjwg3v2dh2j0yjhcja4fxpcen4wg09zmnz6r745wax6heryh0cfl2kt6au9847rsx2t0y2mlk8k9xmz0elpkqj0nurl2jkq3r9dxtf30mt2yezx2mteapgpgtjgdwuc8msu03pap4keqn6nwdl9wtmly6yjhz6fskfh8knjz2rwlm4q9nrfxgfn7v7stckkw7llcg6ufred0azws026dyjydhn5820f4xdreqqdy5af6m9u9jk6l0q8dk0t3wvp3jm0u9pvweqvqd89qlaazve090x7njcu3rp8777t9vdrfwdjj4ensxe34ns9zhdeyrym8x0f44ryy27cc5u88yw4zlyvtenzlurs059suh90jh9dy87xyx3cjlk9g4n995l94awlf29ql42tjm93ytypzeh77j58emucu74h8cr6emauqcalpxhmqthan6md8hn4yjt2vutk7lt57xca6jeaf485hq0jr3y2mpewpkvccs2dp4yk33p7pgutf62kjed5ckctz0dlclp0fayg940e7yt7segyltf5ec7438eadrpkjlq2u3d3ghlxqk30hcjhke6rp4rx5zx8tqxnqcrs3ak9tymc3dmzjz3a3x2ad9z030qwdjr0n5ehc4unqwzs895z7g3u2fhqzkmwj26xqls05hq5rm85cmzphgd2q29pf5sp97vkx7d2l6z0jn9aprtvjp69dtgqj2z6wa25nhpaz72f3d8rrmg58vl0ml3esyh34suktkwadn9jhwv8c4ngte4l8xnstnja9s5egwrmwgrt3dy9sr6ffntszddsa4myegx8czkr0pakjqe0std0uzugevf29wpvf9wvesgwcchfphe2djvzg9wh0fvdj075sgwav8rx72gqx6c8gft08r23e6f2fuqayv3czuctwq6wx4ds0sr88uwl9nlff3xhvej9p7x0mxreq7ktx06myuc76cl9ehw6rhm7284wdaw0ee59xq77558yvm80xp7fc6kwr6grgs8gkwx8e4804pyh492sc3fta4vqanp2n5zndymnywn6x6r4cxl79hl478w52e82ty05zqgy4hgj3j5yq647ca39g5we5hefdd4w3vghfmqr3jfpgm6f5fp9dfvmj3e6j6lfe84adj8xt8p5sex60tyax0ranygaeh6dmr3sct844f795y3zqs0yg94eklhxdlvxrclgnmvzfymhjwcp09seg0a9u3tsy20fum50j5ndu8vqlk9fwd4hwarxjrzaw8n733y8ahry666p4pa87r4eyvtxr63sptrmww7gc2e982mtaud5anhuarpkg24788d0qxv8qz6drhhfp75zhu6q69k5h3zjwfwhu73furpk34yqc0sghqs7xg6jtpnzf4pwdjk0nlv27zf9708kn8zsa7rhae60q0jpmt2e8w5f03vhfq3x2vhyz8w709jwc6p4g3xvewvp20rwe29jjq67qn4jtq3lwztvc9xmcgl89m22ckam4dmk7lse453as63v6nfa4j6q8kzgawfc2qkesvml4zdl8svg4w93877zcckhknucwhwc5rjtrsf9n47kvvn7dvdtqcp4fh0xsqwkr4fqzhfxyq8t0z64dk8h6a9r7dhxwelx4rf6dhmxuwk58lprnw5x8h55ydxdlkdlge5jy2vhu9kgfd7n9qhtf7u002f7r45e8p7v5avh7x45nymy7hvmcmw3rm6nnxqk3c7rhgn0thdgqy6yvek8n5rk5pcj6lec5ngmkk995xpz5dqq6pvp6cws240kp6mc340p4d26cqvgdr5qnx4gsderx79840w6ncuvgyy29nmxa7jkmewzmcaemq5dtwya0qatnzvjtvkuzyauswr9lkgefq4yf46qckuk5rma25vfemwkgtpqhp9fcy8gsg9vp6hmt22mywg7y3cgyfrnd7ltfyms5qxuvme2qpgapfqpyu5ftp65he0ca6n34wmxlxllz0wez7rd3zhrggjc3k92v23rcd8lkkdmjearrcdfsfzlu7kr5vz7wwc4skfnwxsh8sey09suwcx00l38jw50ahhxgz5qr550cy5z263dgv8cew0jxsycrln8eetf8q6t8ewlc0trctr8q73cuell4q4zf9ejfjjqzaq4gnedfs3g2gkcjp5jaf54lq70eua054920map50hjm40ymvt4tmz27mk9ps8d5426y2rx8f4lz92puxv0f865ucmdg2y74x4p5stclk62n7vne0cc7hgwj0tqzdt3dn2r3mc075dn04gl0xkwt5e5dcyu0r5mes7f5pah4ezc7y43rmurgs4kxc2ra5t38gmeemt567w23mxzqsd4u8e4jgvupw5zsc8w6gzdu5k62tr9zk78j0n62m2d6mqrq5zfy2aewftjlmldcv2hwy4q8d7pmtsynzfv7cup8sf8krdq2hjjzjp4muezpuknrfaeq0zzdwrjm4kcz0gdluugn6nv4hgs7up9954wphe05ju2ruk3hnueakkcyknq4s4jnph4sw0e8crmdgdy83g6elnht7x9dsv2wy4f8ueh4vgcy6qp8g4wmcn2hyemw2s5sk5rt5lh", Network::Main).unwrap(), NativeCurrencyAmount::coins(3000)),
            (ReceivingAddress::from_bech32m("nolgam1lkkhj6skt8z0d4zxdxm65hurw9snmj4ljudavuej8w655tmuu8rnmgq7xpdlp5x9jkpsy9ec6w9pjme0xz366hhw27zfrxex370d6mwms3haycx0tt9kzwe9n90vwceaje5954dfkmkjupj0hw4j564v0ggxsqplwwh4zxam6vlhm40c5ehymyxmy7y4tzdyp7xr82rv093f9p37cqk86emy9u54jgea8n9d395n23kgwacr34sna8ydxj84zmnnqm228fklz7mrlwsp7xjqhvgrx30eagxcw4uhsyaerjs6p008c67a5t2eaygwagh3c6rhq2ps4m4alsccwrga0enrnr7m59et9lldx43en9tl9cuykj23r55vrjccr6zytenhlf0gl2yyx5uylxyt7xat8xy3uxplp52tjpjhkaaq64x2ccsxyrsg4n839nywqr9c85qrsjqh4nk4fg4yr564xedgrxa0an5n0n3hy9x9sn5mt7v0hel75ezvyp5v0sgxk6wu5rhjj2pfcs383vqy39nwsh2t5kxljjsau7ww0yqa99dxn8rxej606qvvmfz3j40cycvxp07ks2xxg4gfsg3tv93vlemywv3tz56gchp6ajv6d3txpvq9s0g0s7nk4c4jsm30a8j5whywpzfg3784tpag8rzlahtmvn0n9w756cpk9h8aungmlrs4uwzxlw3tted0fmjqt9885gs5p49u38raleljjyql496xamug7gc4a0gszqxuuxrjl6uvvc0zstwr8atlly74gr5dhwph98ydgr8wmn9j6g9fg0mss2hrts6m2a8fzv4akz6pqjpqclt4d8ugscczmpjarf0n0v0gvwqehupvudx3h4ldl9gqefnaajj7dhhmgme622pgdhknpzdqvvv5mnwp937augqu8hxxplwm8fnq42zeccnp5dumrh6udg5twgylp522tt5246fp9553mwwxdw0m8x4cxczpk2wk9xq7r304p4lf6y34dqu7ncqhvga0fhxdg5and5ed56fkwnlfz0y0s74302cq79jzc7jdvry7j7akqch464xkw68zf53l4css8y23fxu2jup8jgc90qqjqm4qc5qareyq0f7qcddy73lpe7g4t0sw9u0ty57ltt26as2vwftg3wpgldqyvz0fy5tvf0huru6j2uvpdtkd9c0p23afdkdaakeyprkt96r9ku8kdcfcrunkcvv0xtz0ttrwsapz8sy9r5erckk35ewzymwulp0xan6vxnwtypv2w5kg4zmvstevkvmegthpkyv53uphgvzdqncwhq937mzsjxl09y9yuvkvaw3ejeuahcnzpu24e56caemdm7wjwdqx7hmruldqwgjvlm4204emp6a7gw4rg79czgnjwwpuwnwuvvp0s39quaakzp5qmqkmgajr8tlgagtmje9v2qahl64rsaad42xlsgnh5p4cyeqppgmp7zr6g3mnde3vhqk4q5hpl7j2y8nc959hwpvz6z5xep0csna60u79fw67ft90dcvacqp67tprrpve58972uvptpjqk2gg2lgwahjsxnwx323hgq84hg8kfx9njw0a0x0ph44fsjlpaepzy9msh69mmuwq6dxjwma2sekz8pkuj7qlmmjlp533a9ljj8ulh0j8p0eglafs532ddhp9a64ccq6xp3m5xcnuq2jphyuvj97xu4xd5rfsccc3hzp6ydhjxujkddye26jxa9rzx0mhhczed5jlsm4r5ca555hdcrg4y74xs7jqfn77a8cwyee4387mxh72p4rh87429ksthe80g34ttqmdjkhu7u55hpl4cvuhlk0pga3vvyne6hkzcycfqhvqh2k955d0mypq8zwnmnxhqlrxs8zen2d25a48yhh0sxh5txex03evvssr66try2xege90lvh584hsj7z4mv0tkk6qj6hhex72t89yjm9cmj8nlu4z3edw7gnm47yp0v2l7lg7tqm4mqvas5ctuv9qyqlu7d7cm34vcmm753m0yj4jezptn7kzqnlrgqs4cv9cm8etehtknn2r3drpwrxp22v78xycg0m4v37tthpxglsr8rwm0ajarcy9t0chk4m2r5utzfksgp4y2g6xmrmjw5gh0cfd4d4fe5ns8tr4ddfr9ylpez30zjkakg3awr0ymyaxpd7r2za53shkvwqmq88jt7z3g37r40r80qandd422hktxhksrj6gnmvtww49xjexx4xzeucxmd6c0etv02d808m2xwpy5anl83tdtp74kd8cajd9qy5sfd9n4940al3tu8y47zcwfmc5p5mhg6402ps3ykmnpw58tk5f0n50s8l72yfd4e46kwwf4w20es2dsettptvf6zvxmg27jnleepwf420scw0fuhkk0n4hxjvynzwh9x8dlndahj94wgcljrm7jz0sjhxmuluq0q9j4l3z7agsl33hkwvwhhhttufj69srtn7uesdwfdqldf8ynn6eqrfvhf8mvut5f8y2g936nk8cgsxhuy0cn3h9tcnf3wly9e9qv7tvqq0t2eq6j04jvw6qjrwtuym0g5es766zl0u55cu3kzn0mp0feprkc8wkpmk2yrja0fzyk2hsu82vf8kjqdrfj6hxp72ua03z7rf3y2dqaccsaj0d9dq9s0jmpncwukdmgxwrr4p3htqm4ekgy33nx5qldtu47vg3fjv7tq44l9fdvh7zv3tnkmd82lg4lp0ephmp9f7cp4s9h5ml8lwpw6g2psm3hryn3mf8h40v4fnwk8de3svp09a4vgc40lav43hcwx77l0l9hs0ytgerxjjez9cwnryfsxslaednd582m7tmc6t9qhkp32r69csmjrk4gjxj2h5qgszjt3satt28xnans4awdt5vcj0squkntwhtdhxur96n502vcwyajdyqstgymjg2zdc46706qwrm3k9dqdyj7h73mfg5xt90ft0tv7mwtukk0sy6xg0m5mw0jm6tq9va0kkq0z330ypf6equ96jjnxte5yar979fktz5upq7ph3c46g9x58l8rm3f4v3ndgt6u2gldatmx64htmw7wd9rzh70zua7cukn99e3sgxkmeulfmj6z4u70s8tmqpxucv2tv6lv4zrdvgh353fvfxs5a79rrncyz3c5ek88l32mg4x9k3pgf5hcxq4g80gqet9ksv7zqzvq2gxzqrxz8gtma64wgk4gw9ek2nzagtrqcess3wz8uac636lsmts3tjzhcac6rf844h4k5lmk0jjntp94ljus4p3l08mp8adhae2taxqgtxj2q5htucpzxtnntw4muehgpyjl5df8shj0jqpq82674ex5smq3c2hdwxdrzkfy39v4kzk84j7jqawpm0h8dd5j7rachhhc8yyqg7gc72dk3hurnlu7vdm05pzvv95wm", Network::Main).unwrap(), NativeCurrencyAmount::coins(3000)),
            (ReceivingAddress::from_bech32m("nolgam1my4ap87lqyxyxzqzmdxr7mmxyf73cdcuxmdpglcdteg79368l9l0sxn3xs8fr6juaqjjj7z4v4xhq7wuf3zpvnx7nq3yf9snvy8e788hqylnq7dpvcgrszkjxtvhvz3k4d5u5dukp2n7l5xdpxm0z7wfgcad05ezpmx8rxqt9xpkj3jx3ntvhvxnsa8wnn8fd5zkvrgamnfsrdxra5dyhypcf7h03hypu3l4x86f78axukwrgdqdvlh89qewrxkmsmdch9ga75ycveuae4rrhfaguwdrmwpysnzj4rj0d9ahf36mf4palhcs654rd2l2c62l53xwppga05wwc8l59cled0nq03g7u467favv7wa3fael2vrsy0d6hjc3vpkscam6073hmr5ethn7pvdm0qvqtvjt4a240p8kjykwwcnc87djd2w3xxj0lg2mp7cnndz8lhf5c5tgn96lhtpsff9jmaeg7qf3ua5dswh9s3t05xz38g6wwd7ldl5naxdj2d3ad6wj2jw0lxpchylnayqkpejqyltdy6s6jx456ea2gn27csl7xtrwauz9sgvwehm4raezjqvpjnqjzg0e24qz4gu905n8te975rkpy85fwnl69hg88wxec6mlu45ac9zgs0ljtk25eqnx602vlv2pn2ymsldyrlx3wwe85chjy2kna72v0mpuehe3leq9y37pwmmv9qk5ajxm7xf3mtwqnf6da0vn0hhnutg2gpxcyru3767pkqtm5ss29m5kt9trjfuj9eennugjjvlve609emjj8tr6qkcgjfk3q97ekqw3s2sl85ps59vf9zj4zv95drgdrwx5yn873078nr62pwycjr4txlxt6acwpl2lkn5l97uw7umc2dehja8rgx2jaf7n7d2vlmw7udxwzwhjhnhc37h63gg3vwu7kdwyj5eugpjxy50pjdvn8mlmzc23df2ckcy8yaqk0htctacmkjscm8agjs8d3x28xy4ull7kz9jmwm687u93pe77tkcdyg8duw0jy7y8lww4r5mrpgdr8c8errtrhvv84c4443q7yk82cfzmx68lwcxzyjsldyjau6mswg4xmxdqjjqhv6zdm3xavs5cycdnul85xr22hunl58k3u875afy0envnh53lnegxc8y85egtl6whph20qw7cgn5svsdy92z39ep9quk5z0kgxshpy8z4muz88fwfcnmmqh9rl78rc0uskrs6eellpx7n8lmtuwd29vqnntkrgckl4w8h4eyzjad6n43kzhr4elp3a9j6g6y23mfc9j7647lh6dz05w79ujh3eawu85qscmskgwmlx4khyk7pcgl8ss7mnezswspeu0gj2zv5vu4vsrn7nwzlyvag0jnrwd20y08qwnhdghkt9h7ylf4jgczth5dhct8vvwz9aklx565jyjy5hvh25xvww6r94kkg4h3a9m575qketwwh2cxxjzx3a6qve37uc55322rkdlpeldz993mvdrztwlq03avy36cpzxechzjymvzh0h85g8ulxg72wv4j42pv4zwh4zcky8xd4nw35s65sga3nygedzpupntann76st5tep7vj045dph4cdsse8yvsp3g5tmmr3u5qwyxcm86k4ugaxjmnaxdr6733w6fjgmarkyqrzcnwh5m3ckf2e2we74u65vdkn5s7kj5nv3m9ccvxfy8jwyxky2xdsv0rc0grpkht2st525nlz4dq9nzfp8w2n2nqs3x5qaa8ntcrptgzaypee299n9xv002r7s3zrjd3zw2g56uujcgq2atxh2zgsk87z3ndk7rlpyusvccyatnx0avs6xsn6vtd70hagdrl7e7yvzkj7g5vvtsry6jqv3gxc9pre4skwpel5fj3d4qrjg3hry3m8qsv3svzvnsva8wqvnltlvfzw5en4drvww0rhfpxmmjn2lpaa6exhs54n88d608e5rh2cppke8qtsxeqevjp4keularhkqnalwkdn6l4uqwaw2gsx3m85j3narspzqm0n6p820g5wmkjpgf8f549jllchjwr7vsfnsfzujtygal2avpse3e7ag6lrag0uheap6tanzw3qufyr4jc5vr3aeky4kssm4h5mr0vzffarx85jps2zfk65fda7fumkx8cfxdkcrp2wddjgapn7vu88wku74qs8qgy6g9t988c7qeahvupl47k7hvf2kswyjaqrv60t4c2ffuv0rd0md26c9hcscnp52pv2s55sldw9lq00qea9v3nf7fkru3upq4zlhmwtaj5jtjwh5835mpyxeu4hpx5dw0fvgt0lqtzdc89utryhqv6tcnp502zpuq6h2j4hfv2pgkq84cnrx9c95n8lcha6mqnrzfd6vnpu9vxghzfnvzs7tj0c47dgmcvzsqdxmsck0c42fxuffmaxudgvct5sv408yhghdmrd2gmnp7myg6q39fl3lwy9yr4quf7hqyadcmfu2mcc0ugfju6y6hravurdqzmkjjefvq9pkaag7vgf7fml5tsn8ryu3lv98ecrxrkmrxju0s3q3fjez9zmf8f7906jgzsu92qy20lfcv9ed9ryqqjtayj3sx2rk6l985qjrw2hs5erpqv3ea3jfplwudpkxxsehk0gd775ugsjwu4vdqqp7y0nvn3sun65y7htvl9wfs3j9jnq0275kxztdegcg68hv0ny5zgffc3cusp9l32xz4hsceq7mxvnzd2q67gepz03fz7a543kk9cf0g596wlqd3newfxch5caxt82smru9kdpfyq04k5zx6qaz6sm8al5zgsqlrfqmzwm4gmjuqadjnes4phwvxf8sxu3yw84nj6z294tmlrtqdty8lqxm37tqt4w0n766cdz2ml7ap3xruhk0xs2gt087g4ly8j9v06vc8sc99nyy7hun7ha7sx8zr084xekf2fnwxr9v3q93mvw7zq6yvs47y3pqrfaqjusl5rxddn8sdfyc3sd903q0yu9v3v8czuvf2jh5spz83uljhun4p0dkygtasl6khclj3ksn2df4s50r9sg2zmqmaju9n0az6hhkalwlavz88lxnc5hvyq8lr0zz8n0sqkpkpy0nc7mk687aj80evnvkj6vk5ul4mnu0nmqva3995ggd63cg0hmkmprzx9sw07chc4wf3dntwxnwjjzkvenqe6fp0536gfqz9ns25247xgz9xhe3pqqldp7dsxstq6wp3hvy5qwk8uzqpvuvl0y025etd76a5mahj76kekpvkl4r8x7aw52xesnnf0f6uzdd6vsg76yv9hg4wlewmxj0lytfeznys36hxpkzcyk3hw24jkcj4z9yylfh4zeecndskfnzpkpamlxuq5tvzqpu05lrr6lgczwh4u8hl20pnll2g9suualw39hd0j06dkdvrfjscd2zwu2yy0ua", Network::Main).unwrap(), NativeCurrencyAmount::coins(400)),
            (ReceivingAddress::from_bech32m("nolgam1lvw5xajwaax0ys8ejs6pjy3wdenv72fslp3ktg5pwf0laensh6flvza8p3uq38hvhpl28kunwxacskr6csqst3ypjph2nxvqx5k4jfhrvdrhnq56fmvuawulwy7zr2wnrnm7de50sryepjm3ehw5mudxq2svxpaj5e05m3h0ynn2ps4zrcjsvfefhe3qmmhr9ncaq3t8kjhzd6yjcemxnl0emdfgs2w0rkem6edqqq3zz7795axz9wev4x60we00xmm8fuehwywyf04m0v9l3clytyqv6yq2uu2shr2c96kxq5t0kdaejxcxj3qzrdrg8u3528drpfr5ajzn7305q38xy7j0p6tmrx23fav9lx8l87t7zkrt6lceu2ccc4wxm5x77uu3z8rhtajsnc5zwcezrwwavgphpcfd5j8aq3a8kk45qz2gafdmg3hq482r0wp6psshawcuskdaq4eksgnqcmrlzhw6pw7tksezvqgwqjv0uwpzg624c63p80t4ztl96yzng7rpwndrnmpj9tu5jqsygmhy5ke2qv33dgk927kxw5p8r3pvzk6m9qhvj9fa26p3u555fy2st98dgsemtfh4ld0rrwdawdxghlahjw7p8raq3psa5kckgunwvt0xgj7y2nk0fvruttzd3e88u6zcnpaqh83jmktx73p0kjwlpsjwq4xcthpw0mccq40c93malqd3dhcl4ylafsxc7t9sh22tcc2w099nds2e94qvqlw3kgle7ce8mxmdnwgzummjufsdux6esk0tgyx0vq5ecwgcxqvu2l5x6425ajkemefg9vxewdwylsz2ypzan0zyjrmftezxl6vfalre5qq98tqrhk6q3vfcpsh7050ww6jrf833jrla9066c5etm38naefmwytnhllyul7ncgltp239043fp0369w3ddnueyglagyatcw00sqe5cux08fkcnkk5qayzjhsknua2grlhhjgv74r4q7503w5thtk3z5fwm79u0kzt059wedfv30gu66dq7p0upvg2rszlz42jspwm7fhng3j6td8nfshm0zpac48sg7vxrqjjh4z0py4qmfz0eg043ckgxpzvry7e3hqufkduagdcm99mrg9sj0ykkked7j83za9287t9zgxekpxc3392xllyda3aclzvatdeacfeqm2m5ejpg857qsza8fm0xgw2kjvp3fv4f8mz44ldtzlw7ngvsvarj6eqn48x8lzr6kvurgwgfsfk93w7rzatrtgjuktp8ujw2n32ltag5pumfqhw8ccmmzzqt5u457jtmd8eg7ytjtf73ncspnnw7ydhfr7yfdpxxevv9hvvt3wfgnf8fhspjrcvv57dh8qpg879kv7tywl2c55t6g2lcxgt7qxamwy0s7ny5v59572tr0eyxs475axavrq4rqjp60tp8fpk75v6d9vy4xzhlkjtn3fr7zjkvdghtjc2jfzgn2gxq9k5akftucx5wllpxqdj2gymleknezk8jq0zjlzv3dkfx0sa7260s9efmz04cvuv4vz257vzp2ztuumn0gpfk2q7c5r3nnnsdxpxmwrtq9xs4vtqcgs2cjk2tw22f9ezdshxxrm6yctxldtrrr7k3x2h8z99t4ut3zk5uxd8yu8lur7909808jpangj2rxtp0cledv0dlu7xujsjy3h487sv4l66ryz806tcjnnfs7kms5zscza450hhkl74c8u32f622qp7wd8745xpm9rh2q4usjwmxxjr2vaqtp69n2aqch9evay85c8486zjyj87u8x62w3ed5htyfh2agujpue7jqxda5smhslu6tt5363tjq5d9z3wzwak580h4vsrnuqqg6msfa0na4n4clx6mnw2hwdqpj3p3vmkjja5lrvag2xrejdtw74hxcylmwnvkrwqwm9frhnhs8amgeec4cf4gxru0dlkejt88yevkvl3gc65r6gpnppctrfqggjlyk4rk3jc5kdum2ma33vavejzsnpvvv55sy5fuqwt2mrdurtx68s9a9j7cyy55jfnmyxzm9h9hlumlgwad54t8u86t46claymkcu9mfqmeg6wcjx837eed8e3v533wdvj5jv5j30gksn6puyvkvufy85lhvss7ls8j3mpdwnqxl2tjgvf5vcz0h66jzasry2q3u3fdf2uvp05se7p8pcurrjguszk9tmzlad94plzlxqs50hmgqw663s0rvjqu4qa4sujcaalkkfkdnk0cf3s9k9enpuamzgyc73cx52hmyn4hpz520j9unvdu99zrnswk2ac42ah59mje4tgmcg2mx55lwufkdpxrfs3pl86vxpk6qu45vs8gvs0hczy8wyvc0ap7sktxd7lwq3xjvvh4unhuryc0y22r2vvsd5564jlfe8c530vhc09ay5sfn4led5ylsryhnkm5yyrypgsk2jclm8xmvncnsmc0jq9fh3m9086wvwfpm5anjprzpq5d3mcwhp22tl0zxkfmsjz2zxzkv34zkg45xth76xzh27a3x6m3a7y0gffmtfucv09pyqr9a52vtlw69azl89argk0a97y4jx9dcjezsn6etu7c8d5zk3g68nsa9dhrrqv99klegnhne4jgxd50w4hsezaanvxpygpwzrs9qtltgay008qudrg7juawtt6kkjaw5zckhafkqu8r7hs0cvapt58qkjqrnd9kty34j4d3y79v39s34xv3rjjytf9j88tg29wkfw4dl25uppmzrpeknasdjs2r2nvwsd0ht05z9em2gd9ty6dz422067em7gfz38dnntrmqug8x8t8w809udmyepdph5254q3tx35cxrlukxg4q3h0jdgf6lj4f2j6y9gdy6n0qtmqjgtuypwup9gassmhswf9qw5jds6583xnkjjmwu8h85x3wqr5ztgndsvjpen3e6uah97r6ar0ps5d2x36ccyht9wtywds9z675k2cljh8jds8g0vldamhtrlmv99tzr48q622c9vejka5a56lx8c4f3apc52deve470uu97j567ypkcs8ej60m0r8wumd9mrsnsjyhc6unu6fw4flp0rsj29p8khn0t49gcydxfv7aw40vznxp56swxqydgw5dzrdmnegw9mrzzgjvdjt4hxm9zkuh3an2a6efkv0pfccm72j83s0n30ggqacqw57hdjepkvr9whft5wlkkmxtnqluc7k8rv03gey3uxat34nrwqfkfu2mtn022zpx44ygp88r6t3n93948gam4kq0keujxgqjdtmryq89scwhedxypfv0qfhalt9rrfwx3yzwd60nm43yg9t676mlx0en552yea84ee7mxlpey0szmyu6xmntv7sj40u9uq9walqjn69zkcdpx7z33k93ajh2vr6s5w9vjw8ug27avaanh4378q5em4wxs", Network::Main).unwrap(), NativeCurrencyAmount::coins(14167)),
            (ReceivingAddress::from_bech32m("nolgam1v44ly2g8xgnzswvxthx4me35j92fenwzm7lud72eug75ftjyq60vrtpkty5a8acecezpvdh4xxcx9szllrnmepc7v55whmq9dlvzn3l4sdryuwa7acycm8p6p8trtveupela37wcqs3lj6sa0zt7hgsk85hr627ntxm4ta7y2gqjm5tw37xsdju5e52cfey2t8wa8qpu3sjurt2cwjredzz94826ddn6kqts6s84tkldtqsx3p2ujcqgz4txlk8mmx5je2c338hln9mkke3atgl8h0ue2m2xvdx737ja982u6rjrl95qmrxugux4htn7ngduwdk2ufteqnz0dfzt7fa0drlladk7dalfnqtugpey8c9n0nrkpmaf7nk8xsyl3y45yuxs8hzae8lq7z86jaffjkv6thzts2xc2yh098k6pmq7p286lz6swvjd5fayezcflsmxf3j7qgh5kdjqkkzx08nuxmq0l2hzwpegmkzurmwuy5ymjzzqdq84tyy3asy724wt07jfh49gqr02aye0u4v0pljl3erdu5f4dh5pctx3aczfsfy77e3mzsax58enywg3z28lvl9tu4j4h7rk8galnnxtge84ta9weqj0qj8l2shrrar7dfanwzrn3dt5s6s795mwt84ql2exll39xl7trjetg066gv080dz8qmuyxjfmltqh9lu6ycnc33ckd0g6n65aagv0kceztf62w2ufl0ge969j3deeepmd2xdgcxy6agvprrsccvvl4h328yvlycxrmvxvmn2w0jggr6sypdj9wyxndlvql3gfgk9npftfnhdt58p48v4qwgxd4gqqlve9smdn5pwngj6kp4q4sgj30rfjwwl6fkmsndqs87wsl3a48s2gurk4cdquusydd6pflk3pkclqq2klenlzj2p5cznuqye7kasr0srv9hlkg94w9rxzugufchth4ak2a3dz8swdd53k24pf7re285rpr4sfhj3q544hayxp20asf6r8hkvmt052mcu3tmyfze5u7ff67nnvnva7vetwgzy3wrv38f2fzge3f66ha9e5jc9x7cgjpspu37jdzvu9f3lep0r7e7nl05e9p77n7u9nhhda7zyvkvwd8x3jjr4q3gemzytmyqntesatfsalw4zc9anuy2kx6a8g0ej3nqws9ts985ma5xlnzygl7z3lwvkq7uxcnkkwkn5e4j84srtjqkkx34lw2nkw72c5nm9l4vfa7e82lea3zjfpmh0eexmmdz2rtx6etanaxemk97zqmdc2m2crqq0g6gsyhlm9wspyvxp2pztk666h5h5lgcvklccxpyc62g7z77r48d5n8sv6t757nemx0lzm0e95atcdrvmya7xdcry6r3wzkhm7xnsrlq4tnp9wggxu50fsv8ex4ev2theftmxfux8ql0twnnlcra9eay5qq0t4fa83y73upqldveu65gfcjet83xzngs5f68glufm9l6s2d8lhffqf54ddgkh95pjrn35fcp7xfchxnqwtpf4wkmyfrech93zfgqtmzuu5wl0jfaapmttlu0amtwuzxht3jn2gtfdu4axaz68fd5rcj7llw42yhrsttceyus29yk0yrkc05ekjz230pl8fw3ed87gxpjcznujzq8gcmqez0kv40uwkh24tc70tdg22n97jv9rdp5nr7th3h3mzpnz9v6k08tywnyfamh027p80z9zt8stky6k2yu6r4dxc5dq9nhv8z94xhh0pz3r7qrlrw8aa9wngkdt8675gqfzykguncm5p48kectzrh62jrjamacql7fj6aqlxefjw5gve4dac7m0hgcarr6kh73r9hqdapqm6an5uvkvx2nmw8rxchfm2rvq9vws2smx4r5wqdlnl2fc0mw6n8j2p4r76n4y9ye292dugguxf0ahcrt8ezlspxs35ufd6992vv6x4swdp56trkcyqgschv8u5nukxu96qdshycnp8vz47fqwkf9zjkk26kwanxa5y7yusv5k6fj9mchkzluulz9jzvz9apc8h4asgfqsux7z5akj5zpu5wydfu2r57e2826pu48s5e94mk5njlxtdrr8aypg7am8rawqfku4quwlp4fgseth3gtnkdnmhjukz532pcwa2jyjy3etwgya0glg7wszmgw4zhazhh7t7kg5zuecwvj9newq2zql060dlcclnukyz9czkpnhqhyyqsqzytsghfmwj2cnqfpwqw8zcr80wwg5cne7mynnsjj529kyeean5ua2s7w9wza8q9tv5pfwx0x3r69634yy447lyvatuy8n62ad8c0keve2cr9y0ewt5agzmrs4tgqanups69aq6yh0assv8r27682pd3xck0vdeezypk84zsv8974q0paf2lzc9ua68raljl3tzkhtue2zxafyfdqnl692tsxzemzpc0sylp4xuhy8w4ppxrf44mlqwsql5pu3weg6hz3ewq2t2zqmpzrn5v8anmdzsays82nfjfs5hltrnqmtj4ca7cwk0xpzhfg9m39klgk37l5g2mz0uqrzk3ykvrpm4aj7tqx3952eqthxurja9g7t5g8n4jelv4dy6sym7cxz6tf3p0txmdxt7sqjj446pszngw6n9tpv3uwrclp7lsrd2ymcx27x7flv4rq7cxt8r9k7yudkezmapxzfjl5x932lqq0zccejq5yvc0xhenn5t3k8954snu4lmehd2we93achehpt834xe2gqp0yfsqjrahxcmje9a8l7uprndxvderm2dvays9xy0nwzcqxjflvy44ckgfn0kmewlvku8k2d6hz9zhzdkz6j3a34q4nrs0xrvkt7lp3sc2eruqkrrrz3wn9pn6mgpqd2nkjxccqqzn40z8u2gkxk826tfxp57205h0pu48zyvsq8rrdkldrwr82z7z5xg7plmm59kf5d5f8qtkrwyx8gr98nuta46gta2f4ygz9dvkaavvnzd8fq3epkn7rrvatjxgz8xkahx6v0j8s9zgtcqgxtdjn4zdr0hwa3dqcjsmp03uqdkrmrlujtpx8tgz0jjqfm0dul2zkut697ac7dv4mkhrtskf525nkkynsd894g9nxfj8nnxxjc23j0luazka4pyf586jvyac0uyx8hq0tnj7tk3gkp8yt4yhswlejlxqzfrrwc3lk2a9j6vk4mm3tywxhx3w54e3kxl2pdzllauet7e89n2m0cl6j23xlaa45a5dfywxy2c9cp9nk73tl66mga2jlv6d8umluyngajdfdwpxkxtp9kykrjz5ff0dtyjwmppwjcs7g7udet2n55xuwfh2hqf2muk834um5hp64jquwqexkfhhw9e0es538qrksnmzq6z8axzgfe082elucmjuq4lmx03vgqkn58n9un2xf5u7wg9df2dcp2mtlfuk58hxfl", Network::Main).unwrap(), NativeCurrencyAmount::coins(50000)),
            (ReceivingAddress::from_bech32m("nolgam18pudeh6za7kd5laj5xfv2ezatpsasf23m8k9rs0wawxkft0gfc3lgcx98ah32x4mn7zen0zdlzlmmghp34saw4003n7y8pdns8ru4g2h6qt5lahtn3msfy0slc8c57304vpdygmlx6e636xgke5p3da9d7k6sqdknf2nd7flgeveqvqywxzv0earpx29ddzczutsqy6j4c28qsjnvnvufc4qklecd98e8vjjcf3lc5yjew0tx9dvn43cvfwrqwfkafhqkd5s65jvysdlm577ul5kw2c4m4m80v5wrtnlj4dnzarvttmmnereetz4gdkgv7ndrfunjw6tmuzwp77p5kl0gv5r4xmdaz94y9cplqqswrel2qghvupwgryn62yd3xzu3pgsnz0z7t5k0066ruxrlnytz6s3w7tar9pnjesymqq8t4gv2e8ew6yhrwea5tsg7tu203zg2xntj6wueq2llxv6x8f99znd9cy7x2a0a42dg36zsyky2kv9xyq900uvkjfuzxedx9qqk7ey28nashmfsnqrn8587k5ucmxat2nlmru45sqhyuem0us5et66jd6u0uuk4rsnffm4k7c9jxvk6fu5ksj6u8x6cfm6r0anv28srcax5m6cgyd5maqfyf5sq628lxnzt732qlslfey74rvstjclpfgmy029vsnlnlsfuzqe06gl6ql6w73lspuq334lpjgu93hw0rztkdy83s7amm6384shfsfn9kvae4yq7vvpwng62708z8d7w0nlfagm3gu3hlanssm937p4sy6jzl748369f2596ywywe53hlhpeqwyc55n8avts08ewsc73ntcj352tuwphvqlkv4s6uuqf90ezsxry6c5k0q3puz5msxff3vxrkt3alvsa2kqzdzgdxp673l8e6qxtt4ed0t6f8664sajnhdsj5ls3akqppke93qkl7u8xgd4vq8qr4ry4zlgt8r5pklp0mzhwuhz0zfehnwfwry5mke5e8dpl7h3s4dkspgqrway9fy96yze2yyefr9nyrjhjtvne6zw8yq2fafjyxdkhppmgug0w5fyknphyj6u9hfp2l6w27c0kgr3aca3tsv9z4r9w309mcax96zq8m6zcsnhyg8ja20eedl7x06l3l8w9mdkaqf45pshq2ydme0lwe7xl68y9mj49yna2d0ty80h5u8hculktr57qvc02ey5qcm4j0ej7xu0st8pv7s0raddkx4js62e235raw0vk7n9jk6nkwzsu3wg6etanl64phgdnqj5qwmakxwtsgcrywr8yrzczg3cw59p6vhjvp9wrcq9rmz3r25gdnlc6dh83gfw4gp0ng988x42lme3sytqvsh0jajtzy0azftgqp8er9yuy29u90c377llh94eys62x6wtjxpdj9wmpjzst0ga7hzecc2kz9madnr56lpngxymcl70xkufle6tat9g4meklzqqwuw0v30m5kr5wlwu8h72eu2y9cmppwyd3zrl46a77k6txdt7rlxlrqf47z7ls6z0f9fyvu99trfdddy3sznp8p3ytfdy3y2zqjjvy7ffu5u468uqxp0ykdaznt9cmvu9n0nezptfmq3e9kwjyqfsqz8wcuckmaa4wxxd8cty2cp8h977gzj4wd6ham7yl09zqmraju6sslkpnx3tpte4r3lz6nsj9hk7hs8amwa090ehlvvyk2evgm5uujgjpdkpufw98vehsc2xxudjc928ddy8h70f7lw5kxwjyg2reanhnjw9qrne3l4tktkk7xfzxvf4hqnnry6ta7n8n0xt48pgknckepyrh32sjf3fvr90v3sa87ksgz7kfh990gntv3yxutcq6mjpjcvxm7z8kw58ew496nem9w36c6qt0ss4e9ha5pqqacxg34sj26lqp45eddmktg8tsm20sjdp7pmepjch4slqh8rqmnrqs6zn4qnx9vnffsyteu0yv5d3amkce92twalsjz8rcud6mj8hqcmhdccmk7akqu4k0psx0pw4evpdt7wsd5n757qtvwldukmtvsq5nn2ju92aa0nu7c9kq8j2rh927tzvhd3nzqzqpfud8ywacvc2xnheyyy5azgk4le7qgels3vnvz75hmh6fz0c8seqzlew655qhn7v4lwda4xxnta68jgh56839w7hwu7eeyq6w7hyahptjyfzd5wdnwad877wat7hyg6fk6n9774v5pc5gtanuupv6ht6axxs4yrf69awhrwdvxzwzzhu8nzxq4scj4rpsrugy0trr65ugnlrzt6cfse692wx5vq27an87phl5l5f3s0lylhmh0gsn0jg7xeggpw850hz3k0fsyy7yjgawf3mu965k8fwp7yae0j7a7sgyn9am4d0fxr6l6yrk8ln3wyustj9p6qcht6anz5hlqm64588h3ycn4p7y4lf0syym2zap0w7nfdzrpz4klvep5dah2dgfk9yyav4u59dxm9rc27mxanwqvn9m76kmqk8r3xx0cnpq0v9k7pwgzyd8rk4x552n8nxw09gghr4k0txec0w92lq4auqnhp9ee6nkhsl4u4ken869m8tu06vuaqf2lucpvrqt2zap2wkkg30rggeqcunawhtw8f2nr6nv60dwhtlttc22punxw8pfdngndymyzy9aaqrrje44uqrkte4vwwyj37fqyxtmjds0r5y5cajjyqv88q96f8dksja6zjjr2z5mch03xpy7p0asm997x25pkrfjz5k3gzmhwcwxasv872xj899upnydl5tjmvvtuuq8yymmqd3k3q4pp36ghxgzlusqq6my96ehgzk2udyr0vnmj8jfxgeuxh5d8596mwugf8sgvydqc3e6kee0trj5c5g8l23phyhtx4h4ukcn589a6q3sxcwnp4524r609uxy7y3zptgnvx3x28lu3q6em5nxv66npednhwr95umlcn4vvczttjll7dgdcvsyws52pcnd05z659ch45w554flptyqtwrw3jvhlryc2la8rs22lcc9ujl40xyrjglagche8plnlmmtwxvgnfyee2dqldwywpdjehs7egsa9j8kc86z9rqv4pxcy46m3j9paa9rwuhyxpj5xtaq42dfkxpaw654fxtjtxhxg6u8ggm76p0ymp226z65enj0lvcv2n8s4337wxg0gt5455yqpdfuu4psnj8wpqqznhpngu7j83l9wulk50rw7qldeesahe0n5a28qy0qejgt4w3wuqdel83npzt9nr4zwcwy8qjepvskxd8f4w6k69ghkpctqn2nxsk86904mx63efnpkl69cst3zt9y8rdf52vl5d70fpn7nk9zhurya6ewsm09yjwzxldnchd3w9mffkdkrrlvynfjnr78dzd7cun4lty6k5tqeekwzfz229xq8nf24rdjdlmu9plvz8", Network::Main).unwrap(), NativeCurrencyAmount::coins(50000)),
            (ReceivingAddress::from_bech32m("nolgam1syut50n8h5f7ery532clcrng6p924w9xmm26dywelurk5thg48dek2qrctvv3zzar7k0lpx8g22fmusj0lgrs0zszkcnlvstv6qwzfq77rup3ek5vn96jk9zlguknc4kg0t3qke8krcgdlg7x4hxyer5xp5hdk5naj0eqt33xqyrstj238ks89wmgxcfhquk7s9zytqcjcqafhk3nne6ec4r78lyew652xgw2c7dltxkrrf9d6clzdtyzx0zcujgn279qy8lyzcryrcj2lk0jn23jeaedtukvevq3tqgszxykl7lzfp056duedmmaewf357nz4xmz54xgsxrnp77zrq6q8xrsk6fjwh4ryv76ktfmee8nwdv76rv9rpwunvlwmssaspq5u43072rzqv5lnydgk29e4cg3ckvh83nrxw2g72rydhrwnz9trmrlck36k2mq72dcfgnkgdwa7xwdplp8flke4uu2xqwh7vgxa9c59qnqezhp77l89lne6364y9tmuapfsjl7hpe643xqtlc2slre2n94zqrqpsxfy4qwrve43gv70yv4hu8w9ad7l9uafj32uvyfqqv5ep3cuvu05q9ysqc6a9ld8v6a78kta46gmgj6lymq85988jcyh6j993qatk3m8g9yrt7u9h23m0egpfyvmu75zz5k65m47uy04mv8z2vqwq4rhtxpzjyhwymyxvutqqv9zgn7w4c99x2lrpza9ugyfw409l34ys9e4yp6lvd3wuqwjd3stwg0fjruykgmlgc4028xltu740sw02dskszvzx2ha4p9php6cygz7c4de7z5sq9q7mgx764hcl3jsfju8clh27quq2wafkp2sp0jydtlsjqws0pdp8ma3ezzqu6d3lrvjtxtry06uwqdljq3vp26y06n0cymguxmx45wn3rae9cudfsxuz8n37x26r7tnp0uwfaq9300rke3vdhjcwmzneqhx2ujn9p78lah9mehkf6g79fj9qytk8mc4090mrrzz7x5hrekncnwaap5sjw5zwj807f0nmajvxwsysgk097camn29xzxvm7ekkckvkdnt08xxs9v2zvm0eap3g6f85skdmaslsz8atyum30kf2g5kwgdta2vsjdleqq2e5lk240hu03r04jxtqqrdwwdr9y2jnkfcgywpygffgpdm6qrgwzx48vk6dxdjx2pme86drkpwkjhclj4kfl3rfvz3wf0fnspwv2zj45u3p7v6n5dt239p2249dp09saef480gdcfmk8gnjl27dgr98f4zhjhsrx2dv3h83s4mxf9x32gxplhr3vqgxj9z7tkmj8dxxt2ylcyzr8yalp4tvd6jtge98rutyhcvtprxjftagwpnewrxd5xk2hqsesu76f9u3r4e2pks7ag2lg63c38ue4vvuymyn9g72nuqkpn6jfll60kqhxnqnw4s7xgltp7ns2wyfc9wz50ay6xmd238g6pfh7fa8gwyysmzf5lykfta8nvu0c6fks7yx8qeh72yzltxnnuuzmn86va6n8tga7ltnz2p229rej3esgy5eur8gp67cl72aghp43s8j4z4580tdp525gr39h2rlftqvzem82d6unk7nyfzsxg6982h7f03gtthfzs5e9gw6xv6x6u5anmzplk9j2z5vyf2dx5m5hgdx7eqec6yc5l69yl6mkn5nrrakmklu286qrtex55z8xk407juvyx99lt38t5hjatef53qrjzur9ltppa6g738mtn80hgca0459cl84lcpmskr7fnxm5m4k6mpkarxmjxymtn5vjnl96mxdwwc7dff4qry9e8658zp03xcwtn8hg6uy5pgnfy7mp3rnkretegwe86u3gy5qzhr4k36xd60w3wz3mkm3jgxyt20vdg0ywv8wg4muge3fezxn33jdscpr3yntuvhk2a5ywcmhccgcrxa68lgl06vd4eqx6dux0y6qykcej8f70vgkhghaj3ajdxlx570v6gr3g65jzr3cu5f9fn87wfe8lss3rcwc4g2kvushs35gc8rwj8yhlwesg3d5y868f0cn3mqgqekf5df67t3yt52zs4vkn9wn3naccp58vxjx55gjcmykgqetdmh8r6tc8cjwkf462fr8ngtmqgg4grkzgrvdzag3ctx3k36ezk79lqlp22xvucypn4yzzes0tjw4759y8zfuntzereeg4yrxjzl6pkm4t8qcz2zjyd8kjt3eavsvxjw85wmn6gmqdq3r5m6hmazes7qypsjsgrmsppcsanggqm4t2avuxvctyym2z3d58pcda2qvscl0unud04hnk05lkzf2asrpjrmus5v9hz9phkahhsq3wzlepak80ek6v95j6thw9lcyevevz8m6tj7fnle5mhues9gdk9pzp4x5840ma3qmqd0trnx2zvv800d8gwcvyqapg0mqkfwezf9dmy7ugfmcfezeaeu3l3g5sts059f7utn0ffdgqrx5pv7vm4tu9yncdjxhrn2rrtqvfud49muhj2vv9srpvmnk5sapwepq568pg8xjkszttdngjvttp6aqdyks5xaqa8q306ch7avqa0dyqwzgq2v80mj65k6qlzcey6wmdp2daxr9rl424yk0euqg9yn72a8prlf3wzqpnayw8fzrg4feyvwqcg02av2qx874t8ffvnl46faf84n7zn3ar50sncm3spz4emx7a5hhrvyggp6qd9kl07rdj8xs4d3rmz6t08fkcu22vxhwj0rzgrf7u6y8nvu4vhg7eskuj26jwnl090uvd78n3qg54ntfgjw4azuuagfclueften22rm8xsnc5zwejxjdeu532v90k2fncn7nm97shjdknt5suhac5zq9yc8gz5rkt4luafkdnwwevdzvldy24dl2jssr7y5qcudd829pzullp5lfmc8ysdmdcm30y7clgvw5gsaf05mw964ufr9nxfp9r5e2js0jcvuz56htpgha4nelufm5z5u90kujyv39mrwvmxsdl0qv5f255ffrtas8a0syr2v9p7jwuzwzz9y8t4ze3adrjckxmvekvyee2d2udh7khpqqus8zg8wcjepw2qw54aul8a6egspsjn9tc4y5d8r3agsjag54lh92pyzzphfpjehqvn9y32klsnmtpvvt6efdf6ykut95l0mm8vqpfsgtpkqd8hx94kjdvwyktt86p9kuv8hztg4wgnqa3xsc7hmqzayvcptnxszwppfd0l0sg0f8gvv73mwjxqnvauvkugp9qn7eavk9sa0r36uazuxpg3l9k2lexp6fuq0992wa5krpq32xht7c4rvl9v8jupmnqtpzl94xm7pkx7hnvh4ywhqyq8txjqk3wugwsxnr7he5rn2tagf2xkk2jpadyz0j0dt76d0tslk64ak7dpdtr5mfvx75cjjj", Network::Main).unwrap(), NativeCurrencyAmount::coins(50000)),
            (ReceivingAddress::from_bech32m("nolgam1wheq56fv8lk7zc6psvn4t06vc0vp3ukdm8nu9hgzvcxs6eg3rv7rtschp436qwsh9qslsmj802z6j7rceppaq60q55q05hz9hfyusdlypmyg4eyjufsy40wwgnd4zcg4w7qslr3gl2mfh65vn5h58p2g29d04lfa0qwpk04lfeudy0v69xznvk84khw87hpu75ueuq6nd44azlpayt2tre76p8s97fype6sflfz0klsztz699nkp0wysta85y8m2fxxyswgymg4nejchj9qvxa7w655tywwmfafkzdxldvtyh3kxmdudwlnmzaugrrxwy8avxqymahwtcrkgxxwwzz3rmsadnhamf440km02dplpd35cfa8etzqpzytal374erzj043w9vajaf762yux5f65pv2l2rf7mszck8was6eed4pl0pjp9rpd664jy6dvs524nel7ys6c6xky5cvf57yewf6k7eua6vcr7endh3geyj8mv3qehxmnqguyzeqe8kpn6929slahzd39dcsjg8cmllyl00qv8k62yt7ejz9czaxjasz5pg6ua8jn22vh66zgvcn8g465j94gswlq70zeq29t9ntx77zg03nrh6enskw9fzcgtzgflhymqc5nqpp5ex38adzycncye2r993hl8kkf9zxkh6lldsqansujcwr9cg7zwkw3u9ss67mzep4qmt9lnn5zhn3w7la2q74av7ptqkfm9xgx4nrnrd0warvs5meyyd50wdjkk7aye5wypshrkjk2gy07fekehvx93fp3g6cfaz3nddx83ry4ncxpcvn0nzlr4v52n7ys50ru2cpvczu66qns7yuptt7pcx5k8gm4zckn0kea9am0wafae8gh2g4vww7rh05cln63jqamezn8w0w03cz98e5nlez6cl5nvx3c8398l4s568kvks9rwnp502dvejd3vwyq3mvhd0m3lkw850j6z8u9r0ldwcklhfshxmt0mw27cqfezy4kg5cep7mjnxt0j0kcjzd4gdcj83e9r5qhycfuxd2nvz2qvd22vkal6xsw33aymkdxxe6gnvwx5pd5nnlk8mzx5qmz4quw3v727vlmfka78w285e6lqjh0wjfyhlmmqt8qtmcxdmlc6hqkrvp35e0ck5y20xsts7x70re3mu7f8sr9xpcypn7u4u0ca0473l4d8gtx8l0ye2fepkxdsjla8cw04d90pvx8ul0e7ng7rkkxs6d89wasgyw0u382tj04f05ksutxqfed47va7qef0gfz3dkr828xgp2kp87srcuj0fxwapdakjvvkttywadf86phw8t36cnxwchhzg40f5ztr5ksynh4femwfzjh9zquhstp8lt9tje4qcee8r2m5mnmmeuxhnkfx46flslsjfag7gwk0mcp55zgpj0ln9hsakyp5xc72acpwupdarcgd7aekpuevt92aakzrw77wyxtfsed7jrk3eah0e8lled6ngk73g0q8n78k5lyuh4vcw4kv66j4e6gg54v7kmd72ln4x4upmpmeaw8dnm5cn6q45t8mrzm062djat3nh7xyc8tfg9h2jf27egfrltem09m2850dxsp06fh66spjkw790zuyqs0nttefr6q6hzt4qkqg8kuywgmnw9r367qsvxy2uknmwm7z6dadrpgyuk7fvujhw08g9pqh0qhgpx0kj92x7vey5et54hygh3nv3x4hxrq0ly28yuv4ehatr9dpsd7zdf2e2tk48kwsshu2gm362w6tm4rryqeq6lgymk0v4lhxv8mxa5aszy5trnlljvfxh36qany88uzyhs6gqqrfvyk0kg4y4cs6qf4vua5x3avnf7tdhc7agntmjlx8et58x7ckvv06a9f32nsfvmx928dkp0sm5wstcq3723sjv5y9f4tcd6hwd0cum8ejej3a3z94fy8p6dy9suuydg4qmnqn5t7kgrvkjejkhy4h97ls8wt6pavq2vcuwqmwf779ds9ulw2pv57dwpczjhcr8pxupyf5jdyc6l5a9cxnjfcqj63v4zllq3wtadkf99hyq2feujr39j6crjum0vfspsvyg5xmqraw3huv43fusck0hl0vjkr4l0z2slz57gtv05yxrf4kvvmhyucwh68vfha74r09pfacrqe0ll9u3x28zpuuaeew5f2fp2msu0gdqw4d2sy48wm5t579lh6davv96aaa7hekx8ea7xcpq24wx56nenulv6nzs09xfnvvw5spk335v57fqc974nvczz05h52as5cxk63lqq38vyjsrqaacp3l6mdp3mvukgy2ff7ks6yp28yfwu7twfes8869agalz9mp42mtqvp2at42lz92qfktpq5ass72cyk7hxz7uj08wwjgyy684jpte86s9ht9xyzkyl920pzj7czjsdaeuc9yqe99mgj960gyye5vypt06rgkjlnslf3ay444g0fps3ehyvtw3ynwckepwfgfut2fususgrcfdy6lwnl2twtlwhkwzrph40ju52tj4x054d9gd4379hxesugfrmqppzec6dktes23g33xvtx9cg5wzz4wr7mkjgq77w5hxchdxpg68ptfk3c8r0dggwm6d0366y74ttpczjj09mcfrw65e6g6fd4388qpcnwz90k3aenax55v7g3ev2jqqxrdkaykl8p2x5mfwr6zxxay3vzjhphf28dgv9rzsfpnlnyxgjyarlwskjr28tqlw83ak3gf9rdf33r2umgk0kkzdh8920n7ge8j89d6rwzum8jzu63lsgs03fvqy98layxtcs78e2frlzdjfu6f9lnu9y3vkaddmzwcw0rqthpa8282p9l5s7cx9gaw2rusedggg4v5ascaauyvarnd4xmfctzwg0rmxxfgwkpkx7pq2aunc94hf92v8y8wft7x2qx8av9gnqz493e2xryzkf0aueyf68wdqalap7wm5dfytm6ecgnvpj0v3tzvgvq6zmfytxawc625c8ld8exx8dk9mca9u9ustw5xcekp5wtfy93tw3jgmh9njl47tzta5h03x8rttqhpudrhjy8ue6t0jj3dgkcs6cgqq0l2zueh5hydxcd0ghnt737vhpz8q7ksmqag6l3mvfqecg0g0r9palw0n5xltrrhtj5f75807ghfll0hetvzn5hys3a9wjap80kq4mxcf03qmf9h6quyqdg0wcg9rwksdqnnut8cg2y0sjwlwkymu7ktmznc9wqhz4pdadnn3awsafrwuj72372scyekl3wtexwx2gqu4e5y7fk97enmnxfm39e0pekgr29rjl8qy58g68fua0t5xxgr07xwm2nayjm4u6hux2p4mdf3edfmdj920j4rt8r9z7npz3gtx94dan4ycrl4mt7qz9lggymqrxlawrsukz9gd8g75t65dtfkcpke9y", Network::Main).unwrap(), NativeCurrencyAmount::coins(103660)),
            (ReceivingAddress::from_bech32m("nolgam1d6zmse3w57paxszmzekr746p4vchw7g94fu2f0ksfzwt50g99999gnpqgdf0hk00ccgd6tc4urltkujw30p7rqja6ztkqa880tm8jqxk782pag482dj6wl2d74njage50n5jnz7995nx6kj0sj78tsfkcspdphcdwaf9y5gjf3df5kymxfqm223kxxh7rs6lttfzyyu3xr8k9vsren9tjwm64fuuy9pnac8mnsr3dwmvw5923u5akdk8tcty352trlyduj9gjq0xsxrfylp52j7lvfxl3kxuvsc4p73nupzw8tc2h3hr3keqs7k3939mpk884t8gmqjns8taqjdzs8l2jnq8yr9gzgayht044nav9qqle0cerkprrdpjla5l6t6c9mkk9pcahgyzhrqg6aaytuwywkn4f3fl5kue5ncweccnylt835mq548m6a7ky5vsv63uf0fz6r8xx99cdt3ntlqxlg37sd98wn3rmkpfmjxz7sqkmd8jku6l3wyxzd5cpemqvdatvn8m46jnkp0lwf6z2g8shdsjew8rkpcrnuk86qkrmpk8uck0zq0megu3yrah8jd9y9gpet8200cv54u6gy2e3sg9rh5s48ks3v8kgxeuqlfj6zxzxuwmqywx9gea7tn9lxlhtv8ez92lqvt4fxurhfmdl7grax340hvt9pvhrukkg9rzfa94m36v8rng6l9tet6fa2ng345aqvnh74t07qpplduxxx7hucn8cth5vq9cqnxqfs8fq2hxveyfhkat7vewqlr63pzr3n2yfrhjrjkg2m05ec5k2dxy6re7pcpsl0wse6s0h8g9x660evcfdjjx7zdd9vylqqlddulrwm84pm3kghzc637xc8hyvm3mj24pddt7pmygt780gsjvkjzzf2q5c0727q5l93jlexjejt0ws8rqp53a96w234zy348cgvje9htgdljpn449ygzt6l2dw46zxpq2k4swyp7km8j7fvqyva9c8dup54zvtay0c8n854cm4acgcnjdc5dxyufjedeah34387valyu93jrhj9ldhqcp588acck0v6pw52zcv9nmmyq6nwzrjwtnez8pumv44chuplq4308rus82mhg98xw22vtz55jm4txwma4tqnkd2dv6flejgmcnyx5nuvgh8ehxthyapnmph059gzlt2mgwwqcccdqkmdrfdfnv5vrjfh9t6v4mlauw9vlcnz29ptgc9usjx96ladfaufl4nxwlvujdr449fpsnx3vnrnrcej8yvy3cp2a3np3a64ukq26phxdkynlmxmzf7f8kwu7uklgch82zaqpsm0pg044t4lfl29wmz8rxyu5q45ccy8jlanuetgttw6yyzxzxy9rmz3ujdr2qx38ynlxeyyyg6w90ndkk2a27crhe4umcacmcymgqrdknlt9glkk6sgn7mu2yqqgk7jk32p9uy2qmw0vrfwmc0rp3rn6gwge6qxth8vjqc6tfkt9te6waz99nvuz4mkhk4zd8qemyp2hxwyeld56wg5tdvntqfcd6qt09gmmywkzd7g8l86pkx0sdew85s7fsn8gfsv06u3c9gllg4vtyq4xzp48wn0ld8gdm8886ddxna8df78y5mmjfhjz7yke7v79cjcqd6p4u5x99rzs0k5h4uhflkrszlnl6s8x7cy3kgsmypn77r2lv6gsql7jnn98ex6vhrlmeg9r93qy9cy6gxavpqtmw9zfkgu9nhsq2389kmugu8knfvx7lxsruvvk6y3cvpg4v6238ulzsh3efhmapc2nwjqqt6n8q84ex56hqvrjvlape20877p3uzj4pj6waz2fn0c3dxc2nsj2hw8gdcxg204fjxxkgxv7n7syel087tr2emfh825xfwumfc53mlwje0k0r6dw7zuqfxua42jr6q8dm2f2pkpcz59025pygcgjgnnsdxmxhn8xm25zc7lxu7s0jqeu9jp97sqxq7qqlz7r2yw8hkjeaes9fhdk66zm8hthh7vwjlvxfz5k7vfd6ug760chl63afaxrxagjh8rufwq8xn3xpj0ur73thanu53dj4lrlh70q3pujlnfljetl4t6fm2ckch7lkyfs8mwg2ycjxc8fprpdy4tsxexa2aftpqsv6wtya34f48tee940egevrslmvhwjxe6vu5jkrml5l4j70l67uuek8wakk47xu7qc9umkrgn3g9zf5lwscsv6wtaakhxlaxvj4664wjz4gwmvvve0yesm9dr4fvpskegeq7yyzf7uskf69ylxmkcl2a7agrcu3j55maprukz0akkdz8d5fz3qa2qxtzg3xjvd7rdzqmceefeuftgay0nrlwfszpl99hex3xgpkvafsefd5nzxa7qq83nueazewc8xd2e9d2fquh8uswz525pfvdmgxwfltj7nzcnfpskzx9350dzwp5qstq2vhvz7xpry6k5l36v9pzp7et9t20mhat0guumaed5qyr5x5wf2auv8zxxa9mcxalmzny2yx66wx66xnzsqay2k5mm9lfxcf4a4nufm5yt225hgt9sw7zjkjquew74uwk5veenfauq0ltqwv2wytdgf8m45y8dklemr685qm799dvhw72pkertfgzdlaq9vhhtkhkewenl3a50dzwqa4wtrwz0zgkavsvv0dfzxmvwe5e2wlwll0j8vqg2ldyzmk7fmzkh4jvwnt67xx88ftgvzl7n4y05z3h2y8sg0fl5dl5qyfnzq4t36ceq0ew4m6adqgxhsurh8tsea6sasa4lqycextfnzg5s0tx9gc8fvzsl8srrydstmf25llz520f0m7y2yynl7rl35ycgyd3frzs5yul279cuz50tdwvkqgytw7a0upc85kwjwhqcpmr567ulft6qzjc6ssnezacdla24yhr69at8ljj7t08kj859jlkeegx5hqg5nuhvwh9edpxce5w258rg29est4s9x89vwcmv9cxl2k969xxef3mul2t2s0k6xe6cpv8wyr7y6hal475paykwl53yt2e7se8xuh48llge02cyrqe39rhlteyl87ta8hx4uvvr3zatvn769xl5wv5qqtjdlhv2y09tdhc69hgwxaakk8rcz069qkz02q8unh9c6sxnxdksdy4mpcnzn3vj58zter0wujulp2n5dmsg2uq5d0mavvaljl7kldu6cfw3wd0flcg2sl5xgqjrv29wymp5d8jgdxhrqq22k2s9a6f47ze39y80w2k9c947zc0rr6pr9a8n5hkfmmsf6erya0d9lyam577zav7cmsgzhsguw46emwq72qz3c5ydtuy6me5eesztldg2sjm6l8sv0arj8h3xsktv8j0ngdsm2flsd7glwefycly5s8z6qwr9000rxacgn8pf2xvwzxs0lmdndkjlxrt777ksk6yaw4w6a", Network::Main).unwrap(), NativeCurrencyAmount::coins(228306)),
            (ReceivingAddress::from_bech32m("nolgam1rl5v9ty508cr4fdjx4fatpmpa5ejtjytmneyfkgf2fd2exrh9rkdjnw6wdqgfj3rh0fnntek9p8cg9u94sxr6n9ra0v9dy2v6jvkfsze2chg8u8sgtagulwvl3tg3kuud8y4nlgzjnmf8kay4j3upfcyw7s8rvxf2vr4rx7xn9vmv6uaucqsvxy4ec34wjhvx6cfk3uaxkxyup9875fpm7n30dk0jgguh7scsdxmv89gw5f57thd6j3hufslwxklhq9swmn5qtfq35k2lkr9zlsxplx6e2scjx3a4fhg9vlgjvv6snqu9jjv3lhjs3tffj46mjlqnhl47efxy4rz2f0kcc6f82rmt4cd4mqtateas6a2gcw8qxe9mksdkfg2a72rmlvcfzrgeqdynv5n7wn25te43x7s5kj9hcyvrh2wkyedvqxvfk3wwamty8a8hmxrurkmhkjj25srlragln7jxnjamay5t54uddl3uqvscpsjzcdr0wxlmgwxx85z3fgctz73cwthsuwtgwnxjklh93k5utq8pr7pfgse3eqkfj4uu3ytpcrw7gwkxz84fd05342cuxws23mq3d47p6s065ap2rfpargymetf6458srr8j63u3edechrrg0lu3mkcq5qs02areec7a4sv46985wzt79384hakfy0rchw7ca7j4xmz0wmgk6d9wc850ktcnwurcd0atwqprf8fra9p79em2pagah3f4jcexnjdxfu9k0vdtp44zg6jth65zuyfd8zz3u0wqgykk39uckqhqm3l09laayru7gq8723w3jp7hpa27xfe6ehcwrf20434c5nfkp74tmjmaukr8g6f0vzc5av45h28ru6pqeukuk2fuswr6wdkfg9q8yexxxxkg9m0mv5wdf2mw8nt9pnc8sa7va8rz6062q0wfvsyrgxvu9q9rlsyg95dkc2rxa5f2hgf52cx3hru6gex9seqtqeshdqevgju5j956fyrs5cm4869uzwkznu8tshnf2dj0nzz3at0c7daf9st3uwpdsgwxldu8wr945pswl2fqf77y0uj8uvf2gyktrg2cka82xkkvuqplfam548t5m7rxyhrlv3v69z6g8ruyq7c07ma8jjku2g4rmdexxqgefagxetu2mkkfm8uqrrnx5jf4jhlzc8qa8d92a4qwtz5kdtm9jnc95tdnw3w5l9mynhawwfhtpepsafv26rfgm28j556lxs3dnsj0lgefuczrtvvl7k3zd44wawq6punwlhptcsh8t4sryqlkwx0rcvw420dh0p2x5dtv24hd827w5ka5yente9aalrl3eh25ywyg8wf6xzj69ascupsnjl0c0pksg88phj3ee47u2hdysyyffrjremz6u6gr09tkl4740n0ku7mdga0gz6pqnxd45nhqq205gl6h6jfaexklpq0v5489s5dzmkxxmvu6yug97fx3ntgfqgm9gyg2qdlcmykh7pm5kyxw4skzhv3ymhlrexh6ggeqhaxgwvrwl6ssutmyhv008cdmfw005dffna3ggxn93vzzlsj3pa2epfqtnj28xkct75wha0qlmyqye4edh2rlwplq8c0yyfjkl3mqqsfqsupg0ssqwqggvk2yyffpqp3kts99q5alzwvkw96nwkpf0d0w0q9ts0xe9vwu4me9uemtdaxv94q9he2p50nlwkjm8vfddarnzdzjelt6xzedcylkmdq025tukwh3h4p3sp35ww3wazfgzvl54wjusqms0dht5savwp7dtnwh33h36rp85tuj9j8n9vrvrxmu7762s2enlt6rspn7ppgg0epe6hsr5whzr2lp8fwggxywvckujyy0jt4r39c7zt2kclpkeezayekeqdf8twdqypvlm3kygewtktsvw5583t9srkmfhlp5xhdgmgp3qucs2xmqp9ylq57w446k4jm3qaehmqha59vxrepre7654jpls2h494jyl3nkfaan57snfz62qdk2qhu3f3zkt2qtpvydtvpmjxjt7072x90uwdfg2x7v6ew0y63qva647cnn9r8ey5y0wz8pe6q7mxv02u0h3d4zgn9w4kjay405rxcetj54tc2r5vwdzhes3nstuhgekzxasqnrtyfhrwgt4ylr3ewu20awl7atj3luwu64n6nn4spum7ua34eh00l8s882hldc8rpmra30vv6sxqrg367md60hefc8595mfzx3flj8u4pgdxslh566ncl9dqzgt3ctxjsnmmpg24l08dsuhw8g7jswhktrnsvaycw0jm27a7t944txkn8hya3m63muvrlcxcuulk6hx094ht644al7fcnylc4ewvtrtsfmmcva8a3wlwmvxm2dcaafsdnwj4cdr7xtzyg0dpym8ehjgxklvrjaf237taj58pt32ftxx3t4llgjm74ft6w68l8mruzn7yfxxmjelqe02ugx94ueqrz6hd2zelf49x4qs59uhdge3kkv0ydveev93ma3xqhvs3pu3xr6s7ytwk6f84xse5uru9sjkckrstgk2s70tfnn589h87rajps9qjxfqd9d2cafsrtfs4pngjllnrlj9mzd2m6j6d36zxqxrst0fu8sqrrptw2wfemx6tkqta5ln6swgal2l6qxsylf94pe2pl0j004yy26467r9tqg2d2yajvhgr4ku2me68gtt0dl029yh0udlhzc4yc455d696nehg8t6ygr6e48ya4a2fpt8yv32rr6t9p850dtrqp6ju02y0nxalr8f5v3rg2e79twgum6g6p7h5ccknxsqs4a0r0grv46xjpdwgz74pe20cq7yqnwakh5chwfzdthkdg75s27g3jez9e3n3qje00wyphgyxxhc68jtqjfh7g5srmcsf2hnk4e5kxcepsmjrc64e5zs9pxu9s3uhh6c3zsnx7k5p9nfcyyz87l2dus5gayd0k4upwf0gkdyyec6uraw489e602sxe82u0wfuh995r80mzyfewarzv9kzk03tuw9yradjv9g4masc8wxmnu5rc29nwh948s59fhc3emmy3u7sd5pvxg0z2425vrhuk3d3w4mk9930f25cdz2wt3r7a9zjfja38l889uudfxtu8xhqrq0e4hk0z99x2ff0xhk50mpatppetyqtw902fn4ecrvxxqedxy9atdr66lvps5tulss52tnwwhux3edza5yxnt6v3vkftwzgqc7qar9adn2p4kvthu6csy9742g9ddehf3sv2luadvqgvx9uc0sldakkazwnujwyycxq0sqhcgl7npcekravvjescupl578wpxxu7clxdas5y72dmj64e0mmevgklwvk7rj7vjc0zsdgkgc8w2qzp4l6h5mzujtljmf3qfc0qpx0m235jtt9qpc3sdj3w8ku9gnp3qr3zvyag96encr3dtkw", Network::Main).unwrap(), NativeCurrencyAmount::coins(15139)),
            (ReceivingAddress::from_bech32m("nolgam1k6h9x2nyrl5kv89xut339dlqr769v4tz2gd9ky3e9nnxr3zkht6xr9clvt57fejzx760s50ve5l23pg8jtca3xwq6penz2zqx2wffj3fj5h7jml0upyt7qhyn2s79lwuk74eprjxlnyaz8h4t8w2k7q3zkyhxm2g9xhvwjutq7v3gelgwsfpdehx03elncyx5xql6e4xnfdnsl3js9ce7w3wpl6mly4f5tvfp8hcu7t4l8xcjm8pj8x9qfz5wknphrd8vaugykwcjzl526placy27dd2lwy0ynysjgejemvhrjcnwmkn2zez7fhq3uahfnawh4g0usdagwlmttzvffxgp4yqd7c3el2s5jpdqycznpc86p5zs5fvxsudlmq0r3mffz8vfk5dx9jpkvz4v5kzz0xnj8pkah2f23k79t7pwrwgs8gupzpfp2rc9zptw9fpnlsm85phkgep8w62w7xvttzlzth84klzzmt8mta623p98w2a9jgcgc2p2tlcag9j2zv0f6fp89jk40krc3qshy6rxs8t2jgvngzcj2qvlp94trdlj3qwaxyvhz03wa03fsy06n4qrk80t79wj9pexv7eswgtzgcdc5gry6f06mhtcp7nygvsplkvdq4ecujm7062gsv2rtwrve0uy3pkn3hxfjula7xddrypukj43der474awklsx7g3damcp8x4kqa4wj73yl27yuk9jhs8qc0xa3vs4ws9pkw4ehpzhmvypmq8hm09fwvmvm39zrgccfwe0l0qcv40eu7uesne8ty49nqz769l5z4ejs9trmc9q5p45sdv24k00ul2qr8hqff7fwk0855s0msgyvx0xchqly8yed9uf07vrz7z42f86sf3hjq7098cpphu06vuea30dxfj9hg4glhl2exndlnftqvqyj3na6hhycdank7sr7h97m4zqy8mnthd25j4graqjuhd0lkcc29mt4k4esmdttajtma5ruzu3kmtkeztakw5jgxlxz6ku6ywqfkyvspmr96shd8z73e8xr2gs35z76jq80mqqjkq7zl834jsukqzw5h6chqxhrazddmglvuqa2yqxt20d7l2a6dy85nfvk8vp78gcw84t3av9dza7vtl78l7fc950a0y03asqcp707635swaex3daf0gxvcpuz2yty3egep26rgx3kmvqxcz30wk6fr24vsgnt4q3d8xgs860n5p72v9g66u8rpmtkv3cc4mcr5gqkae8j5lt5ce86tjrflva548la9prexpluu7sdy3xea7adk5fum4p8y96tn9axpapd2c922f26mt58ecf87q0fp9zesz6n58masq34hhd7y97v7n3z7mand3ytu4e9e453dkx8rdpm8hzyc6avy43fu4ug667jehtdytnl2dmaeaxcuyhk768dcv0r880ypqv7a8d3cxjgrymsc9638nz7xppn539tz3z6dqf4pu49lt2nmkvvz70wndp3xewnakcke53wnduqhmp22wq3jpgkhkms30x5wvsan2f43gyelnhz4gyaq8c6nauk0h7xyag5q5uqxkkdtttahu97njhweds757qgg6j5hv4k6uevuvf7v0lccqe6633687glprnnjksqaqrwp6mjuz5wg73jlkpug5jmnssjz63fqmkpa4ynukuessjv7zj2an9vjqg64s4k2zkpn4htv87ljecklpfyrcqt3r772mjx4kwj3at75rejkm23anu8src4krttlk4rr45kfsssq0jq732zpekjtp7gq6slhwqph0kzhqqp45pxq88uljaa6rap6sk3qzz7xn4q4d3r0hhy8cusddtsn8za9hgtcqjx2fkuwnxlaqpejzzmtlued4c5gm8r3vha7zr5264n3tgrmpafz3up68mltsalcr8nrtxs4zets72uaw8394ea8amsnzzwlmjuy7mut6tkrrq2e57r97mcxlum5qqsnwv0fkcyapdpjt5wlur8hhtgqex7r8wfrc30ul66va69mge42w7q6ak3xc28r88mngmuhhzsmp26z2e2y4pjg76hqxvtktazvc7g3e75vz6kknfeppd9z7xvhknw95gtytv4hlm8pnya5gs5cf8xd9t8fcyyg8k8flcfnlqh495ncmtvt7f8fhd7rhaq3r4ucj6vkvkyxu65npf3afycc9s665l4cd0kawmggn54a5fv79aqwv596qklz5pkmqyer3g4tzqaf4j4qtwlmqn4sdja7tyurm2kp6f9t6rtuquhtnn0eg5kz96uu3gcs2kq0a3k5dafjpdp44tzepxgfr8dg3n53yft55pky6gdnql2n0x3r6whyqw2yhz87zkcr2jg903g69s8texc5qrzn6l02q7c6626l8wycv9jjpdd7ckllx9dhhwv6kscfmpcjksghc5ypvvkpxvqjxzp5vgfsczvl87fqfnyatg66lve3srr2v87wu22suf4hum5rw2h3y4gewwm6rsru6qak4tlldzsdddt739mx0u2ac49dt4upu7x2keqlhxt3g5e2fmvs7ezf4fxljm72sp2y9avf5qgcuze302ll7wmqhramp0j0u5phxvxwqtftc6n00kf63m0fzyngg0jq5ruhn5nr8zzfqkhchtzd5jem0gfls4d8sdq8htj2kkmftf9mjqe3a740rwclw0fnllhl44cgmg3xdsz0urr68npg0zuk6wd02clwtnmesak3eagepdmfg6fpr7ppskkq9kuwl5s8nccdrd8ctktd5h0t3jpdyt5swe3me0a9p8l8y5ekflfaxg88rrhkc3fj8x9fg5txll9fu0drr0dcq6ph6sumgfn5e56ujasdnss5l3gs5e499l5gqx0c6wngy8g9a6z7zuar33qe93lp99tuf5jemfgnydxqewf6hf8g80pefyygf2n5j7ayagqkg8r5n5yv7qnxsnq0w32pkyf43e9ynjr0kp6a40fcmrqwtp5lsfepnfrtfdu0rea4wkteh6cl4qm3myjmx79xm7qde48ntu3cz76wg6d0hcuscsyfx4p9609z7d7kxx7zjn79mcgszyh3syp4402q2v0jtznt7xl444wtlsjr235gtyvfu30pedcmes69ku5mdvlynj34td720tvd6qtk6k7udlvw4sk0hza3ekg6s09meflskwa7nf4k350shrmf907tuucfrt88ec4e896eel6yqgd04c34aftd8dgeathk08jkys5dp32undugscwm4jkz6kdryxmhv0889ez2p0r79kp7p7hgscsrgt2m8h2hvf904p068q0m7wft7fnreucwm68xgwvc7emdfnqrphqf4s4u525zj0k4fyxwwgvc62hk4mm6qps3jzma0zcyw28hl6s7ex8k30r6j9d5jkylaevtnfwkl45ll3mdgt3h83l48z8e8tjl", Network::Main).unwrap(), NativeCurrencyAmount::coins(72723)),
            (ReceivingAddress::from_bech32m("nolgam1vdsth6nlnxpmduje5aqgl0cvtkzffrjy8ax996nhvkug93dt5qumeulr6sxazvf2gzl22wwy5gjm0r4kjlkvyuuw068rp6ckmx8p9d7thzm5qp3cxwhf0ku0qn3xsv2jqvnfsrjr5tjufrs62nvjucu3jj90sk58psr4t4eczpc3j2l629qewlslwsskxkehpvwptvu8vjdgfv8966xhxrtjrseean0kuytuxtkuhx35wjaf76q36km7gjxun6hanp8mjsay94xjmvjldntlrmll7g9ajj69ut4036rh65jy5qzar5n5z83zruc9wgc6mnyjhrk93q4q87dhdk9e2acl0pwgrwcvjhnk0hh5d767ps74jmw3n38v4c7832ryv9vsl2cqujp8ejgwurx5v2m564tdznmdrs6dk63v099lwrttjfp2lhwpqchj83shscwz0xfxn2g56y485sve3x4v6f75rka63qew2hr7zrqyyd4rmgf7u3c0jtza9e0dx82cz0dn5dpkujs3nvdn54mt0tqmdpnjvdtjskv9xpak8xklg05muqfjdjaep89dm82h7wuqznq4u2f05ecz7c6gtrw8dpvz6ms9usv86xd87lcnlx93zvs6yssvkw7kzgdlpmjjeed20nuu5e36zpjepj5g52jml4pwf6vja40zpq3ynyuk6efzzav72qt9qxl7653yx25hwtx6ahg56rqhq2hzw5pg7rgmp75frk6ldrgtx0z5tfkudu95epxcv3wrsrngw6nnhswfqx7q2yw03rjv8x4qswgea2ux0naer3l6sgdf48yy67z6zta37u4np2hyawtxt9ssj6pjdqgkzjgjlnfz8m9jc2sumcnw4dx9czjh7e3fkgqc8azqx6k544jdh6jxgggckxg74qkqtypkwmvwl4gwqredn5jk3zwlz47nxwtjwtrdlp5qguhj884tvkuezds59phg7jnfu027c5jccwxrq7mgtxzkfa4myk9hm42eh55u58en9auwmc97wx6zms00vewmf3u3g5uayzxq0xwenr93c6rwdfugf8jqet5c73g0k663wzv4eqkvaulkczsshgydjsjj745wff2z4ktyry8upquc0y0rcctl8kfyfhazqnd3qt6apuv3w4cpfg3wx4uypshzlnnajds5elcp2pqhkd56e8v84689hx8wanjd2z4qq6evugn025rp2x05acn95zycqqecfuj86qtlpkakfrnf4pfyv0tq0nuc3y2fz3wc4qr2ck23vaala0f9jrqk2c8m9r5m7cya5c08t0nsu82xlr9kruzv4r0j5880t0e4e644nsdtyqxr5vzzm5zhx67cde7y6hv5jycuvvnusa9r9j3c94947gfggjkjwymfsmdnupzlqxkwh5ysljzvvj6fv0s0welss0st5derpxdmsrac3s2uthctq86djd6ds67vvaxpw5wrenltwe7rpau7m6phehlqpj2pgze79mx39uzdrd4q9d2v88gexzn0kufsq9zayhl4ky5dfc2hlxya3rs5vpsazrf93ndqjlyev82kawpjxdszm0szt32uvg6eg5vqhd24t0e68mfgj9w5yy2tlxtxelp6ysxmgkmxwfx4rvwy34wrjx6c3sf52gawfaqfvfqpx2cf82yk34kyhc4zv4cpuraxk0mmll64lucqdn3a53a0l9yy2p7dpr7u9r3egzvypuwrw9vnhfdynwy0glgn4pk2gyksvnjme637z6n5x9yupvk0zjtdr5zh6ny9ushq73ccp4pgh2hsrckn7tq70up60kmccf8pchqk2jjmekt7j2vevjhuf3498jp3xmf4m0xzajpgfed5v5njaqsfhpxwhdgzzcaktttax7al9ywla0ss04e2tajt9nxks3jycfz6hyat2r5tww2z8huy57d4kuek9rry6j4h77g26zp6muc3jqdtvkpm2nhefp499gyl8r88r6tkldt6gnvxgaaj9p6qm5z25f0vewdeppl5vls6lnng04um762krgv0xshykutv82d5yglscmu2qtz4jzk9qzust3q5uef24hxma700sdrgkd57zmw8hv97ak924s2asfaw4s7yj6ln3ewa39yfqej7nfvttsql54ha0xyd09lyy8wgyuvrgz09mykxflgy2mxq3skudwcj76ay3rggdmz8ksslv9y0frleqlsc9epnrw33t3fefqf7vwplap9eynjlhlqgjes75p9hdntdk06sj9872vga0k8ut8av03lwh5hzfxtzxn83q57fkurryp32kuc7qfrncy4drcwddwumplhlxlwa4tjldpz46wam3fuxxy22a46rsx9mxjghatzu7ah3dktew95h479cf29myzvp5ddnqk5prve7q74ajfqxwjkygrjndu0zhlcvhjpnfvd5st6jfydvn5w40qhcvmw5yxgqnxquv0s40447z0zq6ell20sgdqfylmkjuuk4f3usu72yn3gjjsscysysj69p4lcspk6048uk6z7dfm0nnwf60ru2suyv8cj28lvfskh2w9jspzuja8jm4nk0nar4h2a8x0dr9m6xnl3jnc84pw9rz6hwnmv8zn4pr3sde9lk6nrjqcmt8fp37a284ftkkzh4h5segcsxczg4uksq0hrvm6jcfl4ace8jhlwjlesjkd2yxfue5v2uk2qcj58rna2kp3luwt58sap7guufng5ugdq3nkf06jhqgxdyz6jcq6t3gv7au0mmx8g0u76z5qthm9qdmwgd228q72e8xhxdjrmehjudfr2x8hrt87ey574lkurme5nh8nw0pw7samwmuj87qk4l853w87nxsjxsymqlhdqh76cvd2stlpv2xyk5fhc8jqu9m3hld3fvnvhzhcwjl64edtrkpp8crulm6vnerkuxfpp4nl6ytj3qnvxqfxwjrrffgcr0cv383vurrj0qfwwkhyqzlcl9gh4s9lln4c0s25sa4n6schdm664zud4vhlkgahvtmwqq37nngw2xvd28m90u68lmwppnlcn8mz7dsd42xwkz3h3m3gkf0xr3097pzaaj2aka4kg52d30uzqd602r5wrtwre4vz8hcly52a06qwy0h2x0jzhgmurq399zrvhdl7umdhumjmeklm6ytm4rv2w53vngys0u9l2lxhvthhrt06vygdzr7q37tnrx2tmjh3uhgqpdlrm2lmux9jh5z5lvgte6fewceersfjewucrvhg7d2c5sqqekpwwvcgyt0rk2qku6pvdd9w6m6lu9ulc5gh3s6qh6nuq7g5ey3xv9n08e0qqrr266zx6vpml2f7yd7qt6h5ahqpkax8d0ywxymyx498nm09eaft5skhnjqkrfv3s8cdu897rx8a6j4ss96mpfg33kavq0nm7h56jymeq0pszujuapv4a", Network::Main).unwrap(), NativeCurrencyAmount::coins(2542)),

            // Added 2025-02-08 in [commit id not known yet]
            (ReceivingAddress::from_bech32m("nolgam1aa8u9g5p2vu7aplsusuhev6xjlcmnl9hqjyg93k9v9psewu4kfm3eaqs6rnknd0hydccm6kpagmwj6m35xcnd0qstzzdkl5k92y97p0mvmccap8pr06csqpd2cvafpqvk4eccwrqu55tustuarkk3vuyn34wp30uny8k53dv8ud9zx94dcjuvhadkw6kmj4pj4xxlg99k68gn9mxcjuvgl823lur6l8vxl96s5thtq5twqx72pmujm343qvcgz2yjedafdlqefras503zlr86va574hrnfmeka95xrp9xexhdq4m2aq3ux4xwuxfxmfhz4dk2r0aejf38slj0tk0vulr87v0ysvpuxymhr60ct0p0scka2rxzgk92vusakyxghry306sw45ztah2qcwd2h7zt8fer69kz4gu8z7daj5fx8n7mzkd6h2qnpjzykhqlk3uzc5fx6yszsgz88yu4vp7eq64syecgsgh0vqvx2r50pm9u5dre7nc68qcr90mmjs6nf9l22xjyuzckqt6lenamfyp63j5j7pm08cz4dcul6unmtdcjlmxj7573vm9hq5q6udzm3lhuk40axn5fxcclaxmeutm9uxqklmg86fdhf7ukj2vef7fq9gy8axpsrq5v6g76rg5tmc4ls2yrledqf7gjwjjv2sdd2m5q6kt4ny3u4vqfp2fmh03wgdpuksxxqaj33xu4lcalg65uypjuauxydqmng9cwml0a97yr8al5u9c0lnugq6jte5p42ffzm5n77tzscy9kudymp0ws8lvutuxkfmuw90rtq3hjxjc96jp50vq0gxwpsvrssfv5jqhqgf3k87pw59fjv7h2x0hjclc94rp2ml555jl350uq7kxtmq7mkduje78maj2wxjz3xlg0gztm4wzs9xq94lk93f23lcjym57g6m4t3ut4culah8pgcnescr8007l6uf7jtcr2rqlndtk93gdqffhtfuvjs56v0djqn8d2ekz8zpcnqu6xz5aljqcumfvcae33tad9z7e2mvm2nx8wj9g6w3ez0qdrdww6f6x2rqsrffw9jn0n2gwu0pu4k6g6gep09n42utlugyqg8vfsf5mfyawmynlsefgj78slydsxcfhqzesz0smat33euqkfcp563m8xttlhqt7dceuw5lc7vmcvz7gmw9hd7q7hldunzgpssgj2phcs60sjx7pux82mg5639lxdlsgjgyjyuuehqxxuvghwngvt7cqec9gkp35sgqqqvcwj2p595h6696azqx3v72xs3un3dgj9r3mn4t2racydynn9nn0k4rcvcw4qunekq7sszvr73jt5stx65kcxzmht4gkp7e5sx832zhnvcwmurnkgndx48zz0rk0uvnkq0qh7yp3crsvkz6362tpc4qh5dhgvj7nrru20ka55xnc3ycgsgjtulpeklgpydf42rdmyzgrk3u4yzfpgwkwculvaarrhar55rpd9xgd22h0wtfcp8j80fvu3v5rzw8eqw6uzyefjhll0f3g33ad4cdrgzv0lrjaq0ln3ana59lzlj95n82qxj9zctklr4de07w58z37k93sxrfcrct3fwugkt7xmgvgxd27lft0dh2usjuuxl6uptw0jjhs5lkcasak65qzk0uyeqr39u2dzv6qpn3zm32kmt0dyj7hrc64udrr7t8q7v2kevexjza8ng7aum5fnyv7hm4gagz4l757zfftstdtvyw7g55mwgazejnnsf54lr7r72e98ssr00xsyjktjshp7nzta07cgpeyxnqacfphka8sl756e3qxy9zffmqwrh4kf8y6xt4zn8ht7y898p7yashsee04rn2gatswy0zg47p0vslsc0d0phlgst93nlkqqsulshs5h66005tdls00menzy4flj0wzkuw8gtke408wkczvsp3kwzmmm925zws5kz6r5ua2grggrxdhk6ygpuy7zufnja978z3s9sv4cnl2cjqr02q7s0yz4uz2qelnsumr06vpvd6fgspt7x22dpd4dmaunahtv8tqyxm7p5725tljztkea04k63pu3c8d6lt7ejetpt06nhnaj2jxdupr0200q06l0zwqrm5f2yw023xytr64vgdz0vn94cruwvjxa7mn4h870lgzqge2k6x38zs8luldt8h4nfhs08q0j5uy0p9vv03zck6l9yqlghe8sprnrraggf9247tk9shcfqhsr0p20stgz7epnqxfknuz8lyvdcs0vdsr8smhxv845ey55ynepvhxtp828px9rspzzn2xdh3ahy234crnnxq83f0lu7rezv7cttjukmnlhfcfsgeqlp2449cjkap8ccj09hqu96zjy2f7kd8xezdthwggq6agtg3mxasuna0hgp6n9t29yzy2q6hf3rdqwfn34advh3sshfkl3wze3mm8cprn2dse588rs8z3lekv6p9y6k87jf9kacg27c4auhy8th6w9u23y5udvtezl2vda0ehtmg9rzxxfygy9qppj48wrjdwhuurqkxjgrxe3f20na3736nuadqym4ny7wk20cv9u5sxxs3fkfy0n96dug0vq8st9hfcp89gkej69r75nsl278nxvhkxvx5tpjlskxnyjgjk3wjuaurps72qqdk6edlkvy9zs8p4jsgl5895q0csqq28pyd0zvuug60neqwagg6xhp5yd4tg4w7jhngz9ua94ryh2suxzwagr9l4s3ufzgvtmpqudssxug94p3yq74nsnht2xlnq6phlkmgcge6sn0jh25ves2jc6hlwx9zt7ec5znvh0whk6za5rz8k2ksuf3h4u6eljq5dspg8jy5frr6ss4c40vc42qlftraal450zrv266xj2c27gmcy6g0tcn59qcrtvx6p2vqt472wv993mu6sgzvlcynqmcjh5qm7smqz8rtyfjajuupxe4z4untw0y2vhvptzv8vf39wz4qtx39pr7vvtpa4nwfaj6u60twf7kyhkhvu9zajyvj3q4hyqn3tys5wurpc756sjgt5auc0v0j7mejed5czc0q0feuzt6ec60hq2zn30zlgfvnr0za8k4hq62h6jcstqvwvesauv4uchest3yc65segqcy7mw5mmh5ledukrsv04y7cq6wtgrdjyycfawhks6y2vuh6g9scgrfzx8pxz6tmltpv0z7hd586skeevr4wf0wve29duz36vgz6984c2xjnq2fyq7z2g23wlklunnxxdvqkkm9mgxpzqdm0p9au3k8azqfprpac22vg7vu2mem0kxkkts7re4eq8z30ulywct2a5tvv5k09h5dmhgtjgt3d5me4tdzjma8um74xc8h8njwgw7546yqpk273346aca4ctkynl02c45uswa6s3530v4xfpxczpecj4yuv3c90y7ld5k3xq7khjrp6t7c", Network::Main).unwrap(), NativeCurrencyAmount::coins(600)),

        ]
    }

    pub fn premine_utxos(network: Network) -> Vec<Utxo> {
        let mut utxos = vec![];
        for (receiving_address, amount) in Self::premine_distribution() {
            // generate utxo
            let six_months = Timestamp::months(6);
            let coins = vec![
                Coin::new_native_currency(amount),
                TimeLock::until(network.launch_date() + six_months),
            ];
            let utxo = Utxo::new(receiving_address.lock_script(), coins);
            utxos.push(utxo);
        }
        utxos
    }

    pub(crate) fn new(
        header: BlockHeader,
        body: BlockBody,
        appendix: BlockAppendix,
        block_proof: BlockProof,
    ) -> Self {
        let kernel = BlockKernel::new(header, body, appendix);
        Self {
            digest: OnceLock::default(), // calc'd in hash()
            kernel,
            proof: block_proof,
        }
    }

    // /// Merge a transaction into this block's transaction.
    // /// The mutator set data must be valid in all inputs.
    // ///
    // /// note: this causes block digest to change.
    // pub async fn accumulate_transaction(
    //     &mut self,
    //     transaction: Transaction,
    //     previous_mutator_set_accumulator: &MutatorSetAccumulator,
    // ) {
    //     // merge transactions
    //     let merged_timestamp = max::<Timestamp>(
    //         self.kernel.header.timestamp,
    //         max::<Timestamp>(
    //             self.kernel.body.transaction_kernel.timestamp,
    //             transaction.kernel.timestamp,
    //         ),
    //     );
    //     let new_transaction = self
    //         .kernel
    //         .body
    //         .transaction_kernel
    //         .clone()
    //         .merge_with(transaction.clone());

    //     // accumulate mutator set updates
    //     // Can't use the current mutator sat accumulator because it is in an in-between state.
    //     let mut new_mutator_set_accumulator = previous_mutator_set_accumulator.clone();
    //     let mutator_set_update = MutatorSetUpdate::new(
    //         new_transaction.kernel.inputs.clone(),
    //         new_transaction.kernel.outputs.clone(),
    //     );

    //     // Apply the mutator set update to get the `next_mutator_set_accumulator`
    //     mutator_set_update
    //         .apply_to_accumulator(&mut new_mutator_set_accumulator)
    //         .expect("Mutator set mutation must work");

    //     let block_body: BlockBody = BlockBody {
    //         transaction_kernel: new_transaction,
    //         mutator_set_accumulator: new_mutator_set_accumulator.clone(),
    //         lock_free_mmr_accumulator: self.kernel.body.lock_free_mmr_accumulator.clone(),
    //         block_mmr_accumulator: self.kernel.body.block_mmr_accumulator.clone(),
    //         uncle_blocks: self.kernel.body.uncle_blocks.clone(),
    //     };

    //     let block_header = BlockHeader {
    //         version: self.kernel.header.version,
    //         height: self.kernel.header.height,
    //         prev_block_digest: self.kernel.header.prev_block_digest,
    //         timestamp: merged_timestamp,
    //         nonce: self.kernel.header.nonce,
    //         max_block_size: self.kernel.header.max_block_size,
    //         proof_of_work_line: self.kernel.header.proof_of_work_line,
    //         proof_of_work_family: self.kernel.header.proof_of_work_family,
    //         difficulty: self.kernel.header.difficulty,
    //     };

    //     self.kernel.body = block_body;
    //     self.kernel.header = block_header;
    //     self.unset_digest();
    // }

    /// Verify a block. It is assumed that `previous_block` is valid.
    /// Note that this function does **not** check that the block has enough
    /// proof of work; that must be done separately by the caller, for instance
    /// by calling [`Self::has_proof_of_work`].
    pub(crate) async fn is_valid(
        &self,
        previous_block: &Block,
        now: Timestamp,
        network: Network,
    ) -> bool {
        match self.validate(previous_block, now, network).await {
            Ok(_) => true,
            Err(e) => {
                warn!("{e}");
                false
            }
        }
    }

    /// Verify a block against previous block and return detailed error
    ///
    /// This method assumes that the previous block is valid.
    ///
    /// Note that this function does **not** check that the block has enough
    /// proof of work; that must be done separately by the caller, for instance
    /// by calling [`Self::has_proof_of_work`].
    pub async fn validate(
        &self,
        previous_block: &Block,
        now: Timestamp,
        network: Network,
    ) -> Result<(), BlockValidationError> {
        const FUTUREDATING_LIMIT: Timestamp = Timestamp::minutes(5);

        // Note that there is a correspondence between the logic here and the
        // error types in `BlockValidationError`.

        // 0.a)
        if previous_block.kernel.header.height.next() != self.kernel.header.height {
            return Err(BlockValidationError::BlockHeight);
        }

        // 0.b)
        if previous_block.hash() != self.kernel.header.prev_block_digest {
            return Err(BlockValidationError::PrevBlockDigest);
        }

        // 0.c)
        let mut mmra = previous_block.kernel.body.block_mmr_accumulator.clone();
        mmra.append(previous_block.hash());
        if mmra != self.kernel.body.block_mmr_accumulator {
            return Err(BlockValidationError::BlockMmrUpdate);
        }

        // 0.d)
        if previous_block.kernel.header.timestamp + network.minimum_block_time()
            > self.kernel.header.timestamp
        {
            return Err(BlockValidationError::MinimumBlockTime);
        }

        // 0.e)
        let expected_difficulty = if Self::should_reset_difficulty(
            network,
            self.header().timestamp,
            previous_block.header().timestamp,
        ) {
            network.genesis_difficulty()
        } else {
            difficulty_control(
                self.header().timestamp,
                previous_block.header().timestamp,
                previous_block.header().difficulty,
                network.target_block_interval(),
                previous_block.header().height,
            )
        };

        if self.kernel.header.difficulty != expected_difficulty {
            return Err(BlockValidationError::Difficulty);
        }

        // 0.f)
        let expected_cumulative_proof_of_work =
            previous_block.header().cumulative_proof_of_work + previous_block.header().difficulty;
        if self.header().cumulative_proof_of_work != expected_cumulative_proof_of_work {
            return Err(BlockValidationError::CumulativeProofOfWork);
        }

        // 0.g)
        let future_limit = now + FUTUREDATING_LIMIT;
        if self.kernel.header.timestamp >= future_limit {
            return Err(BlockValidationError::FutureDating);
        }

        // 1.a)
        for required_claim in BlockAppendix::consensus_claims(self.body()) {
            if !self.appendix().contains(&required_claim) {
                return Err(BlockValidationError::AppendixMissingClaim);
            }
        }

        // 1.b)
        if self.appendix().len() > MAX_NUM_CLAIMS {
            return Err(BlockValidationError::AppendixTooLarge);
        }

        // 1.c)
        let BlockProof::SingleProof(block_proof) = &self.proof else {
            return Err(BlockValidationError::ProofQuality);
        };

        // 1.d)
        if !BlockProgram::verify(self.body(), self.appendix(), block_proof, network).await {
            return Err(BlockValidationError::ProofValidity);
        }

        // 1.e)
        if self.header().height < BLOCK_HEIGHT_HF_1 && self.size() > MAX_BLOCK_SIZE_BEFORE_HF_1 {
            return Err(BlockValidationError::MaxSize);
        }

        if self.header().height >= BLOCK_HEIGHT_HF_1 && self.size() > MAX_BLOCK_SIZE_AFTER_HF_1 {
            return Err(BlockValidationError::MaxSize);
        }

        // 2.a)
        let msa_before = previous_block.mutator_set_accumulator_after()?;
        for removal_record in &self.kernel.body.transaction_kernel.inputs {
            if !msa_before.can_remove(removal_record) {
                return Err(BlockValidationError::RemovalRecordsValid);
            }
        }

        // 2.b)
        let mut absolute_index_sets = self
            .kernel
            .body
            .transaction_kernel
            .inputs
            .iter()
            .map(|removal_record| removal_record.absolute_indices.to_vec())
            .collect_vec();
        absolute_index_sets.sort();
        absolute_index_sets.dedup();
        if absolute_index_sets.len() != self.kernel.body.transaction_kernel.inputs.len() {
            return Err(BlockValidationError::RemovalRecordsUnique);
        }

        let mutator_set_update = MutatorSetUpdate::new(
            self.body().transaction_kernel.inputs.clone(),
            self.body().transaction_kernel.outputs.clone(),
        );
        let mut msa = msa_before;
        let ms_update_result = mutator_set_update.apply_to_accumulator(&mut msa);

        // 2.c)
        if ms_update_result.is_err() {
            return Err(BlockValidationError::MutatorSetUpdatePossible);
        };

        // 2.d)
        if msa.hash() != self.body().mutator_set_accumulator.hash() {
            return Err(BlockValidationError::MutatorSetUpdateIntegral);
        }

        // 2.e)
        if self.kernel.body.transaction_kernel.timestamp > self.kernel.header.timestamp {
            return Err(BlockValidationError::TransactionTimestamp);
        }

        let block_subsidy = Self::block_subsidy(self.kernel.header.height);
        let coinbase = self.kernel.body.transaction_kernel.coinbase;
        if let Some(coinbase) = coinbase {
            // 2.f)
            if coinbase > block_subsidy {
                return Err(BlockValidationError::CoinbaseTooBig);
            }

            // 2.g)
            if coinbase.is_negative() {
                return Err(BlockValidationError::CoinbaseTooSmall);
            }
        }

        // 2.h)
        let fee = self.kernel.body.transaction_kernel.fee;
        if fee.is_negative() {
            return Err(BlockValidationError::NegativeFee);
        }

        if self.header().height >= BLOCK_HEIGHT_HF_1 {
            // 2.i)
            if self.body().transaction_kernel.inputs.len()
                > MAX_NUM_INPUTS_OUTPUTS_PUB_ANNOUNCEMENTS_AFTER_HF_1
            {
                return Err(BlockValidationError::TooManyInputs);
            }

            // 2.j)
            if self.body().transaction_kernel.outputs.len()
                > MAX_NUM_INPUTS_OUTPUTS_PUB_ANNOUNCEMENTS_AFTER_HF_1
            {
                return Err(BlockValidationError::TooManyOutputs);
            }

            // 2.k)
            if self.body().transaction_kernel.public_announcements.len()
                > MAX_NUM_INPUTS_OUTPUTS_PUB_ANNOUNCEMENTS_AFTER_HF_1
            {
                return Err(BlockValidationError::TooManyPublicAnnouncements);
            }
        }

        Ok(())
    }

    /// indicates if a difficulty reset should be performed.
    ///
    /// Reset only occurs for network(s) that define a difficulty-reset-interval,
    /// typically testnet(s).
    ///
    /// A reset should be performed any time the interval between a block
    /// and its parent block is >= the network's reset interval.
    pub(crate) fn should_reset_difficulty(
        network: Network,
        current_block_timestamp: Timestamp,
        previous_block_timestamp: Timestamp,
    ) -> bool {
        let Some(reset_interval) = network.difficulty_reset_interval() else {
            return false;
        };
        let elapsed_interval = current_block_timestamp - previous_block_timestamp;
        elapsed_interval >= reset_interval
    }

    /// Determine whether the proof-of-work puzzle was solved correctly.
    ///
    /// Specifically, compare the hash of the current block against the
    /// target corresponding to the previous block;s difficulty and return true
    /// if the former is smaller. If the timestamp difference exceeds the
    /// `TARGET_BLOCK_INTERVAL` by a factor `ADVANCE_DIFFICULTY_CORRECTION_WAIT`
    /// then the effective difficulty is reduced by a factor
    /// `ADVANCE_DIFFICULTY_CORRECTION_FACTOR`.
    pub fn has_proof_of_work(&self, network: Network, previous_block_header: &BlockHeader) -> bool {
        // enforce network difficulty-reset-interval if present.
        if Self::should_reset_difficulty(
            network,
            self.header().timestamp,
            previous_block_header.timestamp,
        ) && self.header().difficulty == network.genesis_difficulty()
        {
            return true;
        }

        let hash = self.hash();
        let threshold = previous_block_header.difficulty.target();
        if hash <= threshold {
            return true;
        }

        let delta_t = self.header().timestamp - previous_block_header.timestamp;
        let excess_multiple =
            usize::try_from(delta_t.to_millis() / network.target_block_interval().to_millis())
                .expect(
                    "excessive timestamp on incoming block should have been caught by peer loop",
                );
        let shift = usize::try_from(ADVANCE_DIFFICULTY_CORRECTION_FACTOR.ilog2()).unwrap()
            * (excess_multiple
                >> usize::try_from(ADVANCE_DIFFICULTY_CORRECTION_WAIT.ilog2()).unwrap());
        let effective_difficulty = previous_block_header.difficulty >> shift;
        if hash <= effective_difficulty.target() {
            return true;
        }

        false
    }

    /// Evaluate the fork choice rule.
    ///
    /// Given two blocks, determine which one is more canonical. This function
    /// evaluates the following logic:
    ///  - if the height is different, prefer the block with more accumulated
    ///    proof-of-work;
    ///  - otherwise, if exactly one of the blocks' transactions has no inputs,
    ///    reject that one;
    ///  - otherwise, prefer the current tip.
    ///
    /// This function assumes the blocks are valid and have the self-declared
    /// accumulated proof-of-work.
    ///
    /// This function is called exclusively in
    /// [`GlobalState::incoming_block_is_more_canonical`][1], which is in turn
    /// called in two places:
    ///  1. In `peer_loop`, when a peer sends a block. The `peer_loop` task only
    ///     sends the incoming block to the `main_loop` if it is more canonical.
    ///  2. In `main_loop`, when it receives a block from a `peer_loop` or from
    ///     the `mine_loop`. It is possible that despite (1), race conditions
    ///     arise, and they must be solved here.
    ///
    /// [1]: crate::models::state::GlobalState::incoming_block_is_more_canonical
    pub(crate) fn fork_choice_rule<'a>(
        current_tip: &'a Self,
        incoming_block: &'a Self,
    ) -> &'a Self {
        if current_tip.header().height != incoming_block.header().height {
            if current_tip.header().cumulative_proof_of_work
                >= incoming_block.header().cumulative_proof_of_work
            {
                current_tip
            } else {
                incoming_block
            }
        } else if current_tip.body().transaction_kernel.inputs.is_empty() {
            incoming_block
        } else {
            current_tip
        }
    }

    /// Size in number of BFieldElements of the block
    // Why defined in terms of BFieldElements and not bytes? Anticipates
    // recursive block validation, where we need to test a block's size against
    // the limit. The size is easier to calculate if it relates to a block's
    // encoding on the VM, rather than its serialization as a vector of bytes.
    pub(crate) fn size(&self) -> usize {
        self.encode().len()
    }

    /// The amount rewarded to the guesser who finds a valid nonce for this
    /// block.
    pub(crate) fn total_guesser_reward(
        &self,
    ) -> Result<NativeCurrencyAmount, BlockValidationError> {
        if self.body().transaction_kernel.fee.is_negative() {
            return Err(BlockValidationError::NegativeFee);
        }

        Ok(self.body().transaction_kernel.fee)
    }

    /// Get the block's guesser fee UTXOs.
    ///
    /// The amounts in the UTXOs are taken from the transaction fee.
    ///
    /// The genesis block does not have a guesser reward.
    pub(crate) fn guesser_fee_utxos(&self) -> Result<Vec<Utxo>, BlockValidationError> {
        const MINER_REWARD_TIME_LOCK_PERIOD: Timestamp = Timestamp::years(3);

        if self.header().height.is_genesis() {
            return Ok(vec![]);
        }

        let lock = self.header().guesser_digest;
        let lock_script = HashLockKey::lock_script_from_after_image(lock);

        let total_guesser_reward = self.total_guesser_reward()?;
        let mut value_locked = total_guesser_reward;
        value_locked.div_two();
        let value_unlocked = total_guesser_reward.checked_sub(&value_locked).unwrap();

        let coins = vec![
            Coin::new_native_currency(value_locked),
            TimeLock::until(self.header().timestamp + MINER_REWARD_TIME_LOCK_PERIOD),
        ];
        let locked_utxo = Utxo::new(lock_script.clone(), coins);
        let unlocked_utxo = Utxo::new_native_currency(lock_script, value_unlocked);

        Ok(vec![locked_utxo, unlocked_utxo])
    }

    /// Compute the addition records that correspond to the UTXOs generated for
    /// the block's guesser
    ///
    /// The genesis block does not have this addition record.
    pub(crate) fn guesser_fee_addition_records(
        &self,
    ) -> Result<Vec<AdditionRecord>, BlockValidationError> {
        Ok(self
            .guesser_fee_utxos()?
            .into_iter()
            .map(|utxo| {
                let item = Tip5::hash(&utxo);

                // Adding the block hash to the mutator set here means that no
                // composer can start proving before solving the PoW-race;
                // production of future proofs is impossible as they depend on
                // inputs hidden behind the veil of future PoW.
                let sender_randomness = self.hash();
                let receiver_digest = self.header().guesser_digest;

                commit(item, sender_randomness, receiver_digest)
            })
            .collect_vec())
    }

    /// Return the mutator set update corresponding to this block, which sends
    /// the mutator set accumulator after the predecessor to the mutator set
    /// accumulator after self.
    pub(crate) fn mutator_set_update(&self) -> Result<MutatorSetUpdate, BlockValidationError> {
        let mut mutator_set_update = MutatorSetUpdate::new(
            self.body().transaction_kernel.inputs.clone(),
            self.body().transaction_kernel.outputs.clone(),
        );

        let extra_addition_records = self.guesser_fee_addition_records()?;
        mutator_set_update.additions.extend(extra_addition_records);

        Ok(mutator_set_update)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::explicit_deref_methods)] // suppress clippy's bad autosuggestion
pub(crate) mod tests {
    use macro_rules_attr::apply;
    use proptest::collection;
    use proptest_arbitrary_interop::arb;
    use rand::random;
    use rand::rngs::StdRng;
    use rand::Rng;
    use rand::SeedableRng;
    use strum::IntoEnumIterator;
    use test_strategy::proptest;
    use tracing_test::traced_test;
    use twenty_first::util_types::mmr::mmr_trait::LeafMutation;

    use super::super::transaction::transaction_kernel::TransactionKernelModifier;
    use super::*;
    use crate::config_models::cli_args;
    use crate::config_models::fee_notification_policy::FeeNotificationPolicy;
    use crate::config_models::network::Network;
    use crate::database::storage::storage_schema::SimpleRustyStorage;
    use crate::database::NeptuneLevelDb;
    use crate::mine_loop::composer_parameters::ComposerParameters;
    use crate::mine_loop::prepare_coinbase_transaction_stateless;
    use crate::mine_loop::tests::make_coinbase_transaction_from_state;
    use crate::models::blockchain::transaction::primitive_witness::PrimitiveWitness;
    use crate::models::blockchain::transaction::TransactionProof;
    use crate::models::blockchain::type_scripts::native_currency::NativeCurrency;
    use crate::models::blockchain::type_scripts::TypeScript;
    use crate::models::state::mempool::TransactionOrigin;
    use crate::models::state::tx_creation_config::TxCreationConfig;
    use crate::models::state::tx_proving_capability::TxProvingCapability;
    use crate::models::state::wallet::address::KeyType;
    use crate::models::state::wallet::transaction_output::TxOutput;
    use crate::models::state::wallet::wallet_entropy::WalletEntropy;
    use crate::tests::shared::fake_valid_successor_for_tests;
    use crate::tests::shared::invalid_block_with_transaction;
    use crate::tests::shared::make_mock_block;
    use crate::tests::shared::make_mock_transaction;
    use crate::tests::shared::mock_genesis_global_state;
    use crate::tests::shared_tokio_runtime;
    use crate::triton_vm_job_queue::TritonVmJobPriority;
    use crate::util_types::archival_mmr::ArchivalMmr;

    pub(crate) const PREMINE_MAX_SIZE: NativeCurrencyAmount = NativeCurrencyAmount::coins(831488);

    #[test]
    fn all_genesis_blocks_have_unique_mutator_set_hashes() {
        let mutator_set_hash = |network| {
            Block::genesis(network)
                .body()
                .mutator_set_accumulator
                .hash()
        };

        assert!(
            Network::iter().map(mutator_set_hash).all_unique(),
            "All genesis blocks must have unique MSA digests, else replay attacks are possible",
        );
    }

    #[cfg(test)]
    impl Block {
        pub(crate) fn with_difficulty(mut self, difficulty: Difficulty) -> Self {
            self.kernel.header.difficulty = difficulty;
            self.unset_digest();
            self
        }

        pub(crate) fn set_proof(&mut self, proof: BlockProof) {
            self.proof = proof;
        }
    }

    #[test]
    fn genesis_block_hasnt_changed_main_net() {
        // Ensure that code changes does not modify the hash of main net's
        // genesis block.

        // Insert the real difficulty such that the block's hash can be
        // compared to the one found in block explorers and other real
        // instances, otherwise the hash would only be valid for test code.
        let network = Network::Main;
        let genesis_block = Block::genesis(network).with_difficulty(network.genesis_difficulty());
        assert_eq!(
            "3eeaed3acdd8765b9a3e689d74f745365d6a3de57fb4a9a19c46ac432ce419a92fb82d47dc0d3f54",
            genesis_block.hash().to_hex()
        );
    }

    #[test]
    fn genesis_block_hasnt_changed_test_net() {
        // Insert the real difficulty such that the block's hash can be
        // compared to the one found in block explorers and other real
        // instances, otherwise the hash would only be valid for test code.
        let network = Network::Testnet;
        let genesis_block = Block::genesis(network).with_difficulty(network.genesis_difficulty());
        assert_eq!(
            "380df1ec5895553d056acb7a35a6eb9967c893ccc1e7c6e86995459e4d20e4f99800f04c86711d53",
            genesis_block.hash().to_hex()
        );
    }

    proptest::proptest! {
        #[test]
        fn block_subsidy_calculation_terminates(height_arb in arb::<BFieldElement>()) {
            Block::block_subsidy(BFieldElement::MAX.into());

            Block::block_subsidy(height_arb.into());
        }
    }

    #[test]
    fn block_subsidy_generation_0() {
        let block_height_generation_0 = 199u64.into();
        assert_eq!(
            NativeCurrencyAmount::coins(128),
            Block::block_subsidy(block_height_generation_0)
        );
    }

    #[traced_test]
    #[apply(shared_tokio_runtime)]
    async fn total_block_subsidy_is_128_coins_regardless_of_guesser_fraction() {
        let network = Network::Main;
        let a_wallet_secret = WalletEntropy::new_random();
        let a_key = a_wallet_secret.nth_generation_spending_key_for_tests(0);
        let genesis = Block::genesis(network);
        let mut rng: StdRng = SeedableRng::seed_from_u64(2225550001);
        let now = genesis.header().timestamp + Timestamp::days(1);

        let mut guesser_fraction = 0f64;
        let step = 0.05;
        while guesser_fraction + step <= 1f64 {
            let composer_parameters = ComposerParameters::new(
                a_key.to_address().into(),
                rng.random(),
                None,
                guesser_fraction,
                FeeNotificationPolicy::OffChain,
            );
            let (composer_txos, transaction_details) =
                prepare_coinbase_transaction_stateless(&genesis, composer_parameters, now, network);
            let coinbase_kernel =
                PrimitiveWitness::from_transaction_details(&transaction_details).kernel;
            let coinbase = Transaction {
                kernel: coinbase_kernel,
                proof: TransactionProof::invalid(),
            };
            let total_composer_reward: NativeCurrencyAmount = composer_txos
                .iter()
                .map(|tx_output| tx_output.utxo().get_native_currency_amount())
                .sum();
            let block_primitive_witness = BlockPrimitiveWitness::new(genesis.clone(), coinbase);
            let block_proof_witness = BlockProofWitness::produce(block_primitive_witness.clone());
            let block1 = Block::new(
                block_primitive_witness.header(now, network.target_block_interval()),
                block_primitive_witness.body().to_owned(),
                block_proof_witness.appendix(),
                BlockProof::Invalid,
            );
            let total_guesser_reward = block1.total_guesser_reward().unwrap();
            let total_miner_reward = total_composer_reward + total_guesser_reward;
            assert_eq!(NativeCurrencyAmount::coins(128), total_miner_reward);

            println!("guesser_fraction: {guesser_fraction}");
            println!(
                "total_composer_reward: {total_guesser_reward}, as nau: {}",
                total_composer_reward.to_nau()
            );
            println!(
                "total_guesser_reward: {total_guesser_reward}, as nau {}",
                total_guesser_reward.to_nau()
            );
            println!(
                "total_miner_reward: {total_miner_reward}, as nau {}\n\n",
                total_miner_reward.to_nau()
            );

            guesser_fraction += step;
        }
    }

    #[test]
    fn observed_total_mining_reward_matches_block_subsidy() {
        // Data read from a node composing and guessing on test net. It
        // composed and guessed block number #115 and got four UTXOs, where the
        // native currency type script recorded these states. Those states must
        // sum to the total block subsidy for generation 0, 128 coins. This
        // were the recorded states for block
        // a1cd0ea9103c19444dd0342e7c772b0a02ed610b71a73ea37e4fe48357c619bb4fa0c3e866000000
        let state0 = [0u64, 980281920, 2521720867, 1615].map(BFieldElement::new);
        let state1 = [0u64, 980281920, 2521720867, 1615].map(BFieldElement::new);
        let state2 = [0u64, 981467136, 2521720867, 1615].map(BFieldElement::new);
        let state3 = [0u64, 981467136, 2521720867, 1615].map(BFieldElement::new);

        let mut total_amount = NativeCurrencyAmount::zero();
        for state in [state0, state1, state2, state3] {
            total_amount = total_amount + *NativeCurrency.try_decode_state(&state).unwrap();
        }

        assert_eq!(NativeCurrencyAmount::coins(128), total_amount);
    }

    #[traced_test]
    #[apply(shared_tokio_runtime)]
    async fn test_difficulty_control_matches() {
        let network = Network::Main;

        let a_wallet_secret = WalletEntropy::new_random();
        let a_key = a_wallet_secret.nth_generation_spending_key_for_tests(0);

        // TODO: Can this outer-loop be parallelized?
        for multiplier in [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000] {
            let mut block_prev = Block::genesis(network);
            let mut now = block_prev.kernel.header.timestamp;
            let mut rng = rand::rng();

            for i in (0..30).step_by(1) {
                let duration = i as u64 * multiplier;
                now += Timestamp::millis(duration);

                let (block, _) =
                    make_mock_block(network, &block_prev, Some(now), a_key, rng.random()).await;

                let control = difficulty_control(
                    block.kernel.header.timestamp,
                    block_prev.header().timestamp,
                    block_prev.header().difficulty,
                    network.target_block_interval(),
                    block_prev.header().height,
                );
                assert_eq!(block.kernel.header.difficulty, control);

                block_prev = block;
            }
        }
    }

    #[test]
    fn difficulty_to_threshold_test() {
        // Verify that a difficulty of 2 accepts half of the digests
        let difficulty: u32 = 2;
        let difficulty_u32s = Difficulty::from(difficulty);
        let threshold_for_difficulty_two: Digest = difficulty_u32s.target();

        for elem in threshold_for_difficulty_two.values() {
            assert_eq!(BFieldElement::MAX / u64::from(difficulty), elem.value());
        }

        // Verify that a difficulty of BFieldElement::MAX accepts all digests where the
        // last BFieldElement is zero
        let some_difficulty = Difficulty::new([1, u32::MAX, 0, 0, 0]);
        let some_threshold_actual: Digest = some_difficulty.target();

        let bfe_max_elem = BFieldElement::new(BFieldElement::MAX);
        let some_threshold_expected = Digest::new([
            bfe_max_elem,
            bfe_max_elem,
            bfe_max_elem,
            bfe_max_elem,
            BFieldElement::zero(),
        ]);

        assert_eq!(0u64, some_threshold_actual.values()[4].value());
        assert_eq!(some_threshold_actual, some_threshold_expected);
        assert_eq!(bfe_max_elem, some_threshold_actual.values()[3]);
    }

    #[apply(shared_tokio_runtime)]
    async fn block_with_wrong_mmra_is_invalid() {
        let network = Network::Main;
        let genesis_block = Block::genesis(network);
        let now = genesis_block.kernel.header.timestamp + Timestamp::hours(2);
        let mut rng: StdRng = SeedableRng::seed_from_u64(2225550001);

        let mut block1 =
            fake_valid_successor_for_tests(&genesis_block, now, rng.random(), network).await;

        let timestamp = block1.kernel.header.timestamp;
        assert!(block1.is_valid(&genesis_block, timestamp, network).await);

        let mut mutated_leaf = genesis_block.body().block_mmr_accumulator.clone();
        let mp = mutated_leaf.append(genesis_block.hash());
        mutated_leaf.mutate_leaf(LeafMutation::new(0, random(), mp));

        let mut extra_leaf = block1.body().block_mmr_accumulator.clone();
        extra_leaf.append(block1.hash());

        let bad_new_mmrs = [
            MmrAccumulator::new_from_leafs(vec![]),
            mutated_leaf,
            extra_leaf,
        ];

        for bad_new_mmr in bad_new_mmrs {
            block1.kernel.body.block_mmr_accumulator = bad_new_mmr;
            assert!(!block1.is_valid(&genesis_block, timestamp, network).await);
        }
    }

    #[proptest(async = "tokio", cases = 1)]
    async fn can_prove_block_ancestry(
        #[strategy(collection::vec(arb::<Digest>(), 55))] mut sender_randomness_vec: Vec<Digest>,
        #[strategy(0..54usize)] index: usize,
        #[strategy(collection::vec(arb::<WalletEntropy>(), 55))] mut wallet_secret_vec: Vec<
            WalletEntropy,
        >,
    ) {
        let network = Network::RegTest;
        let genesis_block = Block::genesis(network);
        let mut blocks = vec![];
        blocks.push(genesis_block.clone());
        let db = NeptuneLevelDb::open_new_test_database(true, None, None, None)
            .await
            .unwrap();
        let mut storage = SimpleRustyStorage::new(db);
        let ammr_storage = storage.schema.new_vec::<Digest>("ammr-blocks-0").await;
        let mut ammr = ArchivalMmr::new(ammr_storage).await;
        ammr.append(genesis_block.hash()).await;
        let mut mmra = MmrAccumulator::new_from_leafs(vec![genesis_block.hash()]);

        for i in 0..55 {
            let key = wallet_secret_vec
                .pop()
                .unwrap()
                .nth_generation_spending_key_for_tests(0);
            let (new_block, _) = make_mock_block(
                network,
                blocks.last().unwrap(),
                None,
                key,
                sender_randomness_vec.pop().unwrap(),
            )
            .await;
            if i != 54 {
                ammr.append(new_block.hash()).await;
                mmra.append(new_block.hash());
                assert_eq!(
                    ammr.to_accumulator_async().await.bag_peaks(),
                    mmra.bag_peaks()
                );
            }
            blocks.push(new_block);
        }

        let last_block_mmra = blocks.last().unwrap().body().block_mmr_accumulator.clone();
        assert_eq!(mmra, last_block_mmra);

        let block_digest = blocks[index].hash();

        let leaf_index = index as u64;
        let membership_proof = ammr.prove_membership_async(leaf_index).await;
        let v = membership_proof.verify(
            leaf_index,
            block_digest,
            &last_block_mmra.peaks(),
            last_block_mmra.num_leafs(),
        );
        assert!(
            v,
            "peaks: {} ({}) leaf count: {} index: {} path: {} number of blocks: {}",
            last_block_mmra.peaks().iter().join(","),
            last_block_mmra.peaks().len(),
            last_block_mmra.num_leafs(),
            leaf_index,
            membership_proof.authentication_path.iter().join(","),
            blocks.len(),
        );
        assert_eq!(last_block_mmra.num_leafs(), blocks.len() as u64 - 1);
    }

    #[test]
    fn test_premine_size() {
        // 831488 = 42000000 * 0.01979733
        // where 42000000 is the asymptotical limit of the token supply
        // and 0.01979733...% is the relative size of the premine
        let asymptotic_total_cap = NativeCurrencyAmount::coins(42_000_000);
        let premine_max_size = PREMINE_MAX_SIZE;
        let total_premine = Block::premine_distribution()
            .iter()
            .map(|(_receiving_address, amount)| *amount)
            .sum::<NativeCurrencyAmount>();

        assert_eq!(total_premine, premine_max_size,);
        assert!(
            premine_max_size.to_nau_f64() / asymptotic_total_cap.to_nau_f64() < 0.0198f64,
            "Premine must be less than or equal to promised"
        )
    }

    mod block_is_valid {
        use rand::rngs::StdRng;
        use rand::SeedableRng;

        use super::*;
        use crate::mine_loop::tests::make_coinbase_transaction_from_state;
        use crate::models::state::tx_creation_config::TxCreationConfig;
        use crate::models::state::wallet::address::KeyType;
        use crate::tests::shared::fake_valid_successor_for_tests;
        use crate::triton_vm_job_queue::TritonVmJobPriority;

        #[traced_test]
        #[apply(shared_tokio_runtime)]
        async fn blocks_with_0_to_10_inputs_and_successors_are_valid() {
            // Scenario: Build different blocks of height 2, with varying number
            // of inputs. Verify all are valid. The build a block of height 3
            // with non-zero inputs and verify validity. This should ensure that
            // at least one of block 2's guesser fee UTXOs shift the active
            // window of the mutator set's Bloom filter, ensuring that the
            // validity-check of a block handles guesser fee UTXOs correctly
            // when calculating the expected state of the new mutator set.
            // Cf., the bug fixed in 4d6b7013624e593c40e76ce93cb6b288b6b3f48b.

            let network = Network::Main;
            let genesis_block = Block::genesis(network);
            let plus_seven_months = genesis_block.kernel.header.timestamp + Timestamp::months(7);
            let mut rng: StdRng = SeedableRng::seed_from_u64(2225550001);
            let block1 = fake_valid_successor_for_tests(
                &genesis_block,
                plus_seven_months,
                rng.random(),
                network,
            )
            .await;

            let alice_wallet = WalletEntropy::devnet_wallet();
            let mut alice = mock_genesis_global_state(
                3,
                alice_wallet.clone(),
                cli_args::Args {
                    guesser_fraction: 0.5,
                    network,
                    ..Default::default()
                },
            )
            .await;
            alice.set_new_tip(block1.clone()).await.unwrap();
            let alice_key = alice
                .lock_guard()
                .await
                .wallet_state
                .nth_spending_key(KeyType::Generation, 0);
            let output_to_self = TxOutput::onchain_native_currency(
                NativeCurrencyAmount::coins(1),
                rng.random(),
                alice_key.to_address(),
                true,
            );

            let plus_eight_months = plus_seven_months + Timestamp::months(1);
            let (coinbase_for_block2, _) = make_coinbase_transaction_from_state(
                &block1,
                &alice,
                plus_eight_months,
                (TritonVmJobPriority::Normal, None).into(),
            )
            .await
            .unwrap();
            let fee = NativeCurrencyAmount::coins(1);
            let plus_nine_months = plus_eight_months + Timestamp::months(1);
            for i in 0..10 {
                println!("i: {i}");
                alice = mock_genesis_global_state(
                    3,
                    alice_wallet.clone(),
                    cli_args::Args::default_with_network(network),
                )
                .await;
                alice.set_new_tip(block1.clone()).await.unwrap();
                let outputs = vec![output_to_self.clone(); i];
                let config2 = TxCreationConfig::default()
                    .recover_change_on_chain(alice_key)
                    .with_prover_capability(TxProvingCapability::SingleProof);
                let tx2 = alice
                    .api()
                    .tx_initiator_internal()
                    .create_transaction(outputs.into(), fee, plus_eight_months, config2)
                    .await
                    .unwrap()
                    .transaction;
                let block2_tx = coinbase_for_block2
                    .clone()
                    .merge_with(
                        (*tx2).clone(),
                        rng.random(),
                        TritonVmJobQueue::get_instance(),
                        TritonVmProofJobOptions::default(),
                    )
                    .await
                    .unwrap();
                let block2_without_valid_pow = Block::compose(
                    &block1,
                    block2_tx,
                    plus_eight_months,
                    TritonVmJobQueue::get_instance(),
                    TritonVmProofJobOptions::default(),
                )
                .await
                .unwrap();

                assert!(
                    block2_without_valid_pow
                        .is_valid(&block1, plus_eight_months, network)
                        .await,
                    "Block with {i} inputs must be valid"
                );

                alice
                    .set_new_tip(block2_without_valid_pow.clone())
                    .await
                    .unwrap();
                let (coinbase_for_block3, _) = make_coinbase_transaction_from_state(
                    &block2_without_valid_pow,
                    &alice,
                    plus_nine_months,
                    (TritonVmJobPriority::Normal, None).into(),
                )
                .await
                .unwrap();
                let config3 = TxCreationConfig::default()
                    .recover_change_on_chain(alice_key)
                    .with_prover_capability(TxProvingCapability::SingleProof);
                let tx3 = alice
                    .api()
                    .tx_initiator_internal()
                    .create_transaction(
                        vec![output_to_self.clone()].into(),
                        fee,
                        plus_nine_months,
                        config3,
                    )
                    .await
                    .unwrap()
                    .transaction;
                let block3_tx = coinbase_for_block3
                    .clone()
                    .merge_with(
                        (*tx3).clone(),
                        rng.random(),
                        TritonVmJobQueue::get_instance(),
                        TritonVmProofJobOptions::default(),
                    )
                    .await
                    .unwrap();
                assert!(
                    !block3_tx.kernel.inputs.len().is_zero(),
                    "block transaction 3 must have inputs"
                );
                let block3_without_valid_pow = Block::compose(
                    &block2_without_valid_pow,
                    block3_tx,
                    plus_nine_months,
                    TritonVmJobQueue::get_instance(),
                    TritonVmProofJobOptions::default(),
                )
                .await
                .unwrap();

                assert!(
                    block3_without_valid_pow
                        .is_valid(&block2_without_valid_pow, plus_nine_months, network)
                        .await,
                    "Block of height 3 after block 2 with {i} inputs must be valid"
                );
            }
        }

        #[traced_test]
        #[apply(shared_tokio_runtime)]
        async fn block_with_far_future_timestamp_is_invalid() {
            let network = Network::Main;
            let genesis_block = Block::genesis(network);
            let mut now = genesis_block.kernel.header.timestamp + Timestamp::hours(2);
            let mut rng: StdRng = SeedableRng::seed_from_u64(2225550001);

            let mut block1 =
                fake_valid_successor_for_tests(&genesis_block, now, rng.random(), network).await;

            // Set block timestamp 4 minutes in the future.  (is valid)
            let future_time1 = now + Timestamp::minutes(4);
            block1.kernel.header.timestamp = future_time1;
            assert!(block1.is_valid(&genesis_block, now, network).await);

            now = block1.kernel.header.timestamp;

            // Set block timestamp 5 minutes - 1 sec in the future.  (is valid)
            let future_time2 = now + Timestamp::minutes(5) - Timestamp::seconds(1);
            block1.kernel.header.timestamp = future_time2;
            assert!(block1.is_valid(&genesis_block, now, network).await);

            // Set block timestamp 5 minutes in the future. (not valid)
            let future_time3 = now + Timestamp::minutes(5);
            block1.kernel.header.timestamp = future_time3;
            assert!(!block1.is_valid(&genesis_block, now, network).await);

            // Set block timestamp 5 minutes + 1 sec in the future. (not valid)
            let future_time4 = now + Timestamp::minutes(5) + Timestamp::seconds(1);
            block1.kernel.header.timestamp = future_time4;
            assert!(!block1.is_valid(&genesis_block, now, network).await);

            // Set block timestamp 2 days in the future. (not valid)
            let future_time5 = now + Timestamp::seconds(86400 * 2);
            block1.kernel.header.timestamp = future_time5;
            assert!(!block1.is_valid(&genesis_block, now, network).await);
        }
    }

    /// This module has tests that verify a block's digest
    /// is always in a correct state.
    ///
    /// All operations that create or modify a Block should
    /// have a test here.
    mod digest_encapsulation {

        use super::*;

        // test: verify clone + modify does not change original.
        //
        // note: a naive impl that derives Clone on `Block` containing
        //       Arc<Mutex<Option<Digest>>> would link the digest in the clone
        #[test]
        fn clone_and_modify() {
            let gblock = Block::genesis(Network::RegTest);
            let g_hash = gblock.hash();

            let mut g2 = gblock.clone();
            assert_eq!(gblock.hash(), g_hash);
            assert_eq!(gblock.hash(), g2.hash());

            g2.set_header_nonce(Digest::new(bfe_array![1u64, 1u64, 1u64, 1u64, 1u64]));
            assert_ne!(gblock.hash(), g2.hash());
            assert_eq!(gblock.hash(), g_hash);
        }

        // test: verify digest is correct after Block::new().
        #[test]
        fn new() {
            let gblock = Block::genesis(Network::RegTest);
            let g2 = gblock.clone();

            let block = Block::new(
                g2.kernel.header,
                g2.kernel.body,
                g2.kernel.appendix,
                g2.proof,
            );
            assert_eq!(gblock.hash(), block.hash());
        }

        // test: verify digest changes after nonce is updated.
        #[test]
        fn set_header_nonce() {
            let gblock = Block::genesis(Network::RegTest);
            let mut rng = rand::rng();

            let mut new_block = gblock.clone();
            new_block.set_header_nonce(rng.random());
            assert_ne!(gblock.hash(), new_block.hash());
        }

        // test: verify set_block() copies source digest
        #[test]
        fn set_block() {
            let gblock = Block::genesis(Network::RegTest);
            let mut rng = rand::rng();

            let mut unique_block = gblock.clone();
            unique_block.set_header_nonce(rng.random());

            let mut block = gblock.clone();
            block.set_block(unique_block.clone());

            assert_eq!(unique_block.hash(), block.hash());
            assert_ne!(unique_block.hash(), gblock.hash());
        }

        // test: verify digest is correct after deserializing
        #[test]
        fn deserialize() {
            let gblock = Block::genesis(Network::RegTest);

            let bytes = bincode::serialize(&gblock).unwrap();
            let block: Block = bincode::deserialize(&bytes).unwrap();

            assert_eq!(gblock.hash(), block.hash());
        }

        // test: verify block digest matches after BFieldCodec encode+decode
        //       round trip.
        #[test]
        fn bfieldcodec_encode_and_decode() {
            let gblock = Block::genesis(Network::RegTest);

            let encoded: Vec<BFieldElement> = gblock.encode();
            let decoded: Block = *Block::decode(&encoded).unwrap();

            assert_eq!(gblock, decoded);
            assert_eq!(gblock.hash(), decoded.hash());
        }
    }

    mod guesser_fee_utxos {
        use super::*;
        use crate::models::state::tx_creation_config::TxCreationConfig;
        use crate::models::state::wallet::address::generation_address::GenerationSpendingKey;
        use crate::tests::shared::make_mock_block_with_puts_and_guesser_preimage_and_guesser_fraction;

        #[apply(shared_tokio_runtime)]
        async fn guesser_fee_addition_records_are_consistent() {
            // Ensure that multiple ways of deriving guesser-fee addition
            // records are consistent.

            let network = Network::Main;
            let mut rng = rand::rng();
            let genesis_block = Block::genesis(network);
            let a_key = GenerationSpendingKey::derive_from_seed(rng.random());
            let guesser_preimage = rng.random();
            let (block1, _) = make_mock_block_with_puts_and_guesser_preimage_and_guesser_fraction(
                network,
                &genesis_block,
                vec![],
                vec![],
                None,
                a_key,
                rng.random(),
                (0.4, guesser_preimage),
            )
            .await;
            let ars = block1.guesser_fee_addition_records().unwrap();
            let ars_from_wallet = block1
                .guesser_fee_utxos()
                .unwrap()
                .iter()
                .map(|utxo| commit(Tip5::hash(utxo), block1.hash(), guesser_preimage.hash()))
                .collect_vec();
            assert_eq!(ars, ars_from_wallet);

            let MutatorSetUpdate {
                removals: _,
                additions,
            } = block1.mutator_set_update().unwrap();
            assert!(
                ars.iter().all(|ar| additions.contains(ar)),
                "All addition records must be present in block's mutator set update"
            );
        }

        #[test]
        fn guesser_can_unlock_guesser_fee_utxo() {
            let genesis_block = Block::genesis(Network::Main);
            let mut transaction = make_mock_transaction(vec![], vec![]);

            transaction.kernel = TransactionKernelModifier::default()
                .fee(NativeCurrencyAmount::from_nau(1337.into()))
                .modify(transaction.kernel);

            let mut block = invalid_block_with_transaction(&genesis_block, transaction);

            let preimage = rand::rng().random::<Digest>();
            block.set_header_guesser_digest(preimage.hash());

            let guesser_fee_utxos = block.guesser_fee_utxos().unwrap();

            let lock_script_and_witness =
                HashLockKey::from_preimage(preimage).lock_script_and_witness();
            assert!(guesser_fee_utxos
                .iter()
                .all(|guesser_fee_utxo| lock_script_and_witness.can_unlock(guesser_fee_utxo)));
        }

        #[traced_test]
        #[apply(shared_tokio_runtime)]
        async fn guesser_fees_are_added_to_mutator_set() {
            // Mine two blocks on top of the genesis block. Verify that the guesser
            // fee for the 1st block was added to the mutator set. The genesis
            // block awards no guesser fee.

            // This test must live in block/mod.rs because it relies on access to
            // private fields on `BlockBody`.

            let mut rng = rand::rng();
            let network = Network::Main;
            let genesis_block = Block::genesis(network);
            assert!(
                genesis_block.guesser_fee_utxos().unwrap().is_empty(),
                "Genesis block has no guesser fee UTXOs"
            );

            let launch_date = genesis_block.header().timestamp;
            let in_seven_months = launch_date + Timestamp::months(7);
            let in_eight_months = launch_date + Timestamp::months(8);
            let alice_wallet = WalletEntropy::devnet_wallet();
            let alice_key = alice_wallet.nth_generation_spending_key(0);
            let alice_address = alice_key.to_address();
            let mut alice = mock_genesis_global_state(
                0,
                alice_wallet,
                cli_args::Args::default_with_network(network),
            )
            .await;

            let output = TxOutput::offchain_native_currency(
                NativeCurrencyAmount::coins(4),
                rng.random(),
                alice_address.into(),
                true,
            );
            let fee = NativeCurrencyAmount::coins(1);
            let config1 = TxCreationConfig::default()
                .recover_change_on_chain(alice_key.into())
                .with_prover_capability(TxProvingCapability::PrimitiveWitness);
            let tx1 = alice
                .api()
                .tx_initiator_internal()
                .create_transaction(vec![output.clone()].into(), fee, in_seven_months, config1)
                .await
                .unwrap()
                .transaction;

            let block1 = Block::block_template_invalid_proof(
                &genesis_block,
                (*tx1).clone(),
                in_seven_months,
                network.target_block_interval(),
            );
            alice.set_new_tip(block1.clone()).await.unwrap();

            let config2 = TxCreationConfig::default()
                .recover_change_on_chain(alice_key.into())
                .with_prover_capability(TxProvingCapability::PrimitiveWitness);
            let tx2 = alice
                .api()
                .tx_initiator_internal()
                .create_transaction(vec![output].into(), fee, in_eight_months, config2)
                .await
                .unwrap()
                .transaction;

            let block2 = Block::block_template_invalid_proof(
                &block1,
                (*tx2).clone(),
                in_eight_months,
                network.target_block_interval(),
            );

            let mut ms = block1.body().mutator_set_accumulator.clone();

            let mutator_set_update_guesser_fees =
                MutatorSetUpdate::new(vec![], block1.guesser_fee_addition_records().unwrap());
            let mut mutator_set_update_tx = MutatorSetUpdate::new(
                block2.body().transaction_kernel.inputs.clone(),
                block2.body().transaction_kernel.outputs.clone(),
            );

            let reason = "applying mutator set update derived from block 2 \
                          to mutator set from block 1 should work";
            mutator_set_update_guesser_fees
                .apply_to_accumulator_and_records(
                    &mut ms,
                    &mut mutator_set_update_tx.removals.iter_mut().collect_vec(),
                    &mut [],
                )
                .expect(reason);
            mutator_set_update_tx
                .apply_to_accumulator(&mut ms)
                .expect(reason);

            assert_eq!(ms.hash(), block2.body().mutator_set_accumulator.hash());
        }
    }

    #[test]
    fn premine_distribution_does_not_crash() {
        Block::premine_distribution();
    }

    /// Exhibits a strategy for creating one transaction by merging in many
    /// small ones that spend from one's own wallet. The difficulty you run into
    /// when you do this naïvely is that you end up merging in transactions that
    /// spend the same UTXOs over and over. To avoid doing this, you insert the
    /// transaction into the mempool thus making the wallet aware of this
    /// transaction and avoiding a double-spend of a UTXO.
    #[apply(shared_tokio_runtime)]
    async fn avoid_reselecting_same_input_utxos() {
        let mut rng = StdRng::seed_from_u64(893423984854);
        let network = Network::Main;
        let devnet_wallet = WalletEntropy::devnet_wallet();
        let mut alice = mock_genesis_global_state(
            0,
            devnet_wallet,
            cli_args::Args::default_with_network(network),
        )
        .await;

        let job_queue = TritonVmJobQueue::get_instance();

        let genesis_block = Block::genesis(network);

        let mut blocks = vec![genesis_block];

        // Spend i inputs in block i, for i in {1,2}. The first expenditure and
        // block is guaranteed to succeed. Prior to the second block, Alice owns
        // two inputs and creates a big transaction by merging in smaller ones.
        // She needs to ensure the two transactions she merges in do not spend
        // the same UTXO.
        let launch_date = network.launch_date();
        let mut now = launch_date + Timestamp::months(6);
        for i in 1..3 {
            now += network.target_block_interval();

            // create coinbase transaction
            let (mut transaction, _) = make_coinbase_transaction_from_state(
                &blocks[i - 1],
                &alice,
                launch_date,
                TritonVmProofJobOptions::from((TritonVmJobPriority::Normal, None)),
            )
            .await
            .unwrap();

            // for all own UTXOs, spend to self
            for _ in 0..i {
                // create a transaction spending it to self
                let change_key = alice
                    .lock_guard_mut()
                    .await
                    .wallet_state
                    .next_unused_symmetric_key()
                    .await;
                let receiving_address = alice
                    .lock_guard_mut()
                    .await
                    .wallet_state
                    .next_unused_spending_key(KeyType::Generation)
                    .await
                    .to_address();
                let tx_outputs = vec![TxOutput::onchain_native_currency(
                    NativeCurrencyAmount::coins(1),
                    rng.random(),
                    receiving_address,
                    true,
                )]
                .into();
                let config = TxCreationConfig::default()
                    .recover_change_on_chain(change_key.into())
                    .with_prover_capability(TxProvingCapability::SingleProof)
                    .use_job_queue(job_queue.clone());
                let transaction_creation_artifacts = alice
                    .api()
                    .tx_initiator_internal()
                    .create_transaction(tx_outputs, NativeCurrencyAmount::coins(0), now, config)
                    .await
                    .unwrap();
                let self_spending_transaction = transaction_creation_artifacts.transaction;

                // merge that transaction in
                transaction = transaction
                    .merge_with(
                        (*self_spending_transaction).clone(),
                        rng.random(),
                        job_queue.clone(),
                        TritonVmProofJobOptions::default(),
                    )
                    .await
                    .unwrap();

                alice
                    .lock_guard_mut()
                    .await
                    .mempool_insert(transaction.clone(), TransactionOrigin::Own)
                    .await;
            }

            // compose block
            let block = Block::compose(
                blocks.last().unwrap(),
                transaction,
                now,
                job_queue.clone(),
                TritonVmProofJobOptions::default(),
            )
            .await
            .unwrap();

            let block_is_valid = block.validate(blocks.last().unwrap(), now, network).await;
            println!("block is valid? {:?}", block_is_valid.map(|_| "yes"));
            println!();
            assert!(block_is_valid.is_ok());

            // update state with new block
            alice.set_new_tip(block.clone()).await.unwrap();

            blocks.push(block);
        }
    }
}
