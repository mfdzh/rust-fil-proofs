use std::fs::{copy, File, OpenOptions};
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use ff::PrimeField;
use memmap::MmapOptions;
use paired::bls12_381::Bls12;
use paired::Engine;

use sector_base::api::bytes_amount::{PaddedBytesAmount, UnpaddedByteIndex, UnpaddedBytesAmount};
use sector_base::api::porep_config::PoRepConfig;
use sector_base::api::porep_proof_partitions::PoRepProofPartitions;
use sector_base::api::post_config::PoStConfig;
use sector_base::api::post_proof_partitions::PoStProofPartitions;
use sector_base::api::SINGLE_PARTITION_PROOF_LEN;
use sector_base::io::fr32::write_unpadded;
use storage_proofs::circuit::multi_proof::MultiProof;
use storage_proofs::circuit::vdf_post::VDFPostCompound;
use storage_proofs::circuit::zigzag::ZigZagCompound;
use storage_proofs::compound_proof::{self, CompoundProof};
use storage_proofs::drgraph::{DefaultTreeHasher, Graph};
use storage_proofs::fr32::{bytes_into_fr, fr_into_bytes, Fr32Ary};
use storage_proofs::hasher::pedersen::{PedersenDomain, PedersenHasher};
use storage_proofs::hasher::{Domain, Hasher};
use storage_proofs::layered_drgporep::{self, ChallengeRequirements};
use storage_proofs::merkle::MerkleTree;
use storage_proofs::porep::{replica_id, PoRep, Tau};
use storage_proofs::proof::NoRequirements;
use storage_proofs::vdf_post;
use storage_proofs::vdf_sloth;
use storage_proofs::zigzag_drgporep::ZigZagDrgPoRep;

use crate::caches::{
    get_post_params, get_post_verifying_key, get_zigzag_params, get_zigzag_verifying_key,
};
use crate::constants::POREP_MINIMUM_CHALLENGES;
use crate::error;
use crate::error::ExpectWithBacktrace;
use crate::file_cleanup::FileCleanup;
use crate::parameters::{post_setup_params, public_params, setup_params};
use crate::post_adapter::*;
use crate::singletons::ENGINE_PARAMS;
use crate::singletons::FCP_LOG;

/// FrSafe is an array of the largest whole number of bytes guaranteed not to overflow the field.
type FrSafe = [u8; 31];

pub type Commitment = Fr32Ary;
pub type ChallengeSeed = Fr32Ary;
type Tree = MerkleTree<PedersenDomain, <PedersenHasher as Hasher>::Function>;

#[derive(Clone, Debug)]
pub struct SealOutput {
    pub comm_r: Commitment,
    pub comm_r_star: Commitment,
    pub comm_d: Commitment,
    pub proof: Vec<u8>,
}

/// Generates a proof-of-spacetime, returning and detected storage faults.
/// Accepts as input a challenge seed, configuration struct, and a vector of
/// sealed sector file-path plus CommR tuples.
///
pub fn generate_post(
    post_config: PoStConfig,
    challenge_seed: ChallengeSeed,
    input_parts: Vec<(Option<String>, Commitment)>,
) -> error::Result<GeneratePoStDynamicSectorsCountOutput> {
    generate_post_dynamic(GeneratePoStDynamicSectorsCountInput {
        post_config,
        challenge_seed,
        input_parts,
    })
}

/// Verifies a proof-of-spacetime.
///
pub fn verify_post(
    post_config: PoStConfig,
    comm_rs: Vec<Commitment>,
    challenge_seed: ChallengeSeed,
    proofs: Vec<Vec<u8>>,
    faults: Vec<u64>,
) -> error::Result<VerifyPoStDynamicSectorsCountOutput> {
    verify_post_dynamic(VerifyPoStDynamicSectorsCountInput {
        post_config,
        comm_rs,
        challenge_seed,
        proofs,
        faults,
    })
}

/// Seals the staged sector at `in_path` in place, saving the resulting replica
/// to `out_path`.
///
pub fn seal<T: Into<PathBuf> + AsRef<Path>>(
    porep_config: PoRepConfig,
    in_path: T,
    out_path: T,
    prover_id_in: &FrSafe,
    sector_id_in: &FrSafe,
) -> error::Result<SealOutput> {
    let sector_bytes = usize::from(PaddedBytesAmount::from(porep_config));

    let mut cleanup = FileCleanup::new(&out_path);

    // Copy unsealed data to output location, where it will be sealed in place.
    copy(&in_path, &out_path)?;
    let f_data = OpenOptions::new().read(true).write(true).open(&out_path)?;

    // Zero-pad the data to the requested size by extending the underlying file if needed.
    f_data.set_len(sector_bytes as u64)?;

    let mut data = unsafe { MmapOptions::new().map_mut(&f_data).unwrap() };

    // Zero-pad the prover_id to 32 bytes (and therefore Fr32).
    let prover_id = pad_safe_fr(prover_id_in);
    // Zero-pad the sector_id to 32 bytes (and therefore Fr32).
    let sector_id = pad_safe_fr(sector_id_in);
    let replica_id = replica_id::<DefaultTreeHasher>(prover_id, sector_id);

    let compound_setup_params = compound_proof::SetupParams {
        vanilla_params: &setup_params(
            PaddedBytesAmount::from(porep_config),
            usize::from(PoRepProofPartitions::from(porep_config)),
        ),
        engine_params: &(*ENGINE_PARAMS),
        partitions: Some(usize::from(PoRepProofPartitions::from(porep_config))),
    };

    let compound_public_params = ZigZagCompound::setup(&compound_setup_params)?;

    let (tau, aux) = ZigZagDrgPoRep::replicate(
        &compound_public_params.vanilla_params,
        &replica_id,
        &mut data,
        None,
    )?;

    // If we succeeded in replicating, flush the data and protect output from being cleaned up.
    data.flush()?;
    cleanup.success = true;

    let public_tau = tau.simplify();

    let public_inputs = layered_drgporep::PublicInputs {
        replica_id,
        tau: Some(public_tau),
        comm_r_star: tau.comm_r_star,
        k: None,
    };

    let private_inputs = layered_drgporep::PrivateInputs::<DefaultTreeHasher> {
        aux,
        tau: tau.layer_taus,
    };

    let groth_params = get_zigzag_params(porep_config)?;

    info!(FCP_LOG, "got groth params ({}) while sealing", u64::from(PaddedBytesAmount::from(porep_config)); "target" => "params");

    let proof = ZigZagCompound::prove(
        &compound_public_params,
        &public_inputs,
        &private_inputs,
        &groth_params,
    )?;

    let mut buf = Vec::with_capacity(
        SINGLE_PARTITION_PROOF_LEN * usize::from(PoRepProofPartitions::from(porep_config)),
    );

    proof.write(&mut buf)?;

    let comm_r = commitment_from_fr::<Bls12>(public_tau.comm_r.into());
    let comm_d = commitment_from_fr::<Bls12>(public_tau.comm_d.into());
    let comm_r_star = commitment_from_fr::<Bls12>(tau.comm_r_star.into());

    // Verification is cheap when parameters are cached,
    // and it is never correct to return a proof which does not verify.
    verify_seal(
        porep_config,
        comm_r,
        comm_d,
        comm_r_star,
        prover_id_in,
        sector_id_in,
        &buf,
    )
    .expect("post-seal verification sanity check failed");

    Ok(SealOutput {
        comm_r,
        comm_r_star,
        comm_d,
        proof: buf,
    })
}

/// Verifies the output of some previously-run seal operation.
///
pub fn verify_seal(
    porep_config: PoRepConfig,
    comm_r: Commitment,
    comm_d: Commitment,
    comm_r_star: Commitment,
    prover_id_in: &FrSafe,
    sector_id_in: &FrSafe,
    proof_vec: &[u8],
) -> error::Result<bool> {
    let sector_bytes = PaddedBytesAmount::from(porep_config);
    let prover_id = pad_safe_fr(prover_id_in);
    let sector_id = pad_safe_fr(sector_id_in);
    let replica_id = replica_id::<DefaultTreeHasher>(prover_id, sector_id);

    let comm_r = bytes_into_fr::<Bls12>(&comm_r)?;
    let comm_d = bytes_into_fr::<Bls12>(&comm_d)?;
    let comm_r_star = bytes_into_fr::<Bls12>(&comm_r_star)?;

    let compound_setup_params = compound_proof::SetupParams {
        vanilla_params: &setup_params(
            PaddedBytesAmount::from(porep_config),
            usize::from(PoRepProofPartitions::from(porep_config)),
        ),
        engine_params: &(*ENGINE_PARAMS),
        partitions: Some(usize::from(PoRepProofPartitions::from(porep_config))),
    };

    let compound_public_params: compound_proof::PublicParams<
        '_,
        Bls12,
        ZigZagDrgPoRep<'_, DefaultTreeHasher>,
    > = ZigZagCompound::setup(&compound_setup_params)?;

    let public_inputs = layered_drgporep::PublicInputs::<<DefaultTreeHasher as Hasher>::Domain> {
        replica_id,
        tau: Some(Tau {
            comm_r: comm_r.into(),
            comm_d: comm_d.into(),
        }),
        comm_r_star: comm_r_star.into(),
        k: None,
    };

    let verifying_key = get_zigzag_verifying_key(porep_config)?;

    info!(FCP_LOG, "got verifying key ({}) while verifying seal", u64::from(sector_bytes); "target" => "params");

    let proof = MultiProof::new_from_reader(
        Some(usize::from(PoRepProofPartitions::from(porep_config))),
        proof_vec,
        &verifying_key,
    )?;

    ZigZagCompound::verify(
        &compound_public_params,
        &public_inputs,
        &proof,
        &ChallengeRequirements {
            minimum_challenges: POREP_MINIMUM_CHALLENGES,
        },
    )
    .map_err(Into::into)
}

/// Unseals the sector at `sealed_path` and returns the bytes for a piece
/// whose first (unpadded) byte begins at `offset` and ends at `offset` plus
/// `num_bytes`, inclusive. Note that the entire sector is unsealed each time
/// this function is called.
///
pub fn get_unsealed_range<T: Into<PathBuf> + AsRef<Path>>(
    porep_config: PoRepConfig,
    sealed_path: T,
    output_path: T,
    prover_id_in: &FrSafe,
    sector_id_in: &FrSafe,
    offset: UnpaddedByteIndex,
    num_bytes: UnpaddedBytesAmount,
) -> error::Result<(UnpaddedBytesAmount)> {
    let prover_id = pad_safe_fr(prover_id_in);
    let sector_id = pad_safe_fr(sector_id_in);
    let replica_id = replica_id::<DefaultTreeHasher>(prover_id, sector_id);

    let f_in = File::open(sealed_path)?;
    let mut data = Vec::new();
    f_in.take(u64::from(PaddedBytesAmount::from(porep_config)))
        .read_to_end(&mut data)?;

    let f_out = File::create(output_path)?;
    let mut buf_writer = BufWriter::new(f_out);

    let unsealed = ZigZagDrgPoRep::extract_all(
        &public_params(
            PaddedBytesAmount::from(porep_config),
            usize::from(PoRepProofPartitions::from(porep_config)),
        ),
        &replica_id,
        &data,
    )?;

    let written = write_unpadded(&unsealed, &mut buf_writer, offset.into(), num_bytes.into())?;

    Ok(UnpaddedBytesAmount(written as u64))
}

fn verify_post_dynamic(
    dynamic: VerifyPoStDynamicSectorsCountInput,
) -> error::Result<VerifyPoStDynamicSectorsCountOutput> {
    let fixed = verify_post_spread_input(dynamic)?
        .iter()
        .map(verify_post_fixed_sectors_count)
        .collect();

    verify_post_collect_output(fixed)
}

fn generate_post_dynamic(
    dynamic: GeneratePoStDynamicSectorsCountInput,
) -> error::Result<GeneratePoStDynamicSectorsCountOutput> {
    let n = { dynamic.input_parts.len() };

    let fixed_output = generate_post_spread_input(dynamic)
        .iter()
        .map(generate_post_fixed_sectors_count)
        .collect();

    generate_post_collect_output(n, fixed_output)
}

fn generate_post_fixed_sectors_count(
    fixed: &GeneratePoStFixedSectorsCountInput,
) -> error::Result<GeneratePoStFixedSectorsCountOutput> {
    let faults: Vec<u64> = Vec::new();

    let setup_params = compound_proof::SetupParams {
        vanilla_params: &post_setup_params(fixed.post_config),
        engine_params: &(*ENGINE_PARAMS),
        partitions: None,
    };

    let pub_params: compound_proof::PublicParams<
        _,
        vdf_post::VDFPoSt<PedersenHasher, vdf_sloth::Sloth>,
    > = VDFPostCompound::setup(&setup_params).expect("setup failed");

    let commitments = fixed
        .input_parts
        .iter()
        .map(|(_, comm_r)| PedersenDomain::try_from_bytes(&comm_r[..]).unwrap()) // FIXME: don't unwrap
        .collect();

    let safe_challenge_seed = {
        let mut cs = vec![0; 32];
        cs.copy_from_slice(&fixed.challenge_seed);
        cs[31] &= 0b0011_1111;
        cs
    };

    let pub_inputs = vdf_post::PublicInputs {
        challenge_seed: PedersenDomain::try_from_bytes(&safe_challenge_seed).unwrap(),
        commitments,
        faults: Vec::new(),
    };

    let trees: Vec<Tree> = fixed
        .input_parts
        .iter()
        .map(|(access, _)| {
            if let Some(s) = &access {
                make_merkle_tree(
                    s,
                    PaddedBytesAmount(pub_params.vanilla_params.sector_size as u64),
                )
                .unwrap()
            } else {
                panic!("faults are not yet supported")
            }
        })
        .collect();

    let borrowed_trees: Vec<&Tree> = trees.iter().map(|t| t).collect();

    let priv_inputs = vdf_post::PrivateInputs::<PedersenHasher>::new(&borrowed_trees[..]);

    let groth_params = get_post_params(fixed.post_config)?;

    let proof = VDFPostCompound::prove(&pub_params, &pub_inputs, &priv_inputs, &groth_params)
        .expect("failed while proving");

    let mut buf = Vec::with_capacity(
        SINGLE_PARTITION_PROOF_LEN * usize::from(PoStProofPartitions::from(fixed.post_config)),
    );

    proof.write(&mut buf)?;

    Ok(GeneratePoStFixedSectorsCountOutput { proof: buf, faults })
}

fn verify_post_fixed_sectors_count(
    fixed: &VerifyPoStFixedSectorsCountInput,
) -> error::Result<VerifyPoStFixedSectorsCountOutput> {
    let safe_challenge_seed = {
        let mut cs = vec![0; 32];
        cs.copy_from_slice(&fixed.challenge_seed);
        cs[31] &= 0b0011_1111;
        cs
    };

    let compound_setup_params = compound_proof::SetupParams {
        vanilla_params: &post_setup_params(fixed.post_config),
        engine_params: &(*ENGINE_PARAMS),
        partitions: None,
    };

    let compound_public_params: compound_proof::PublicParams<
        _,
        vdf_post::VDFPoSt<PedersenHasher, vdf_sloth::Sloth>,
    > = VDFPostCompound::setup(&compound_setup_params).expect("setup failed");

    let commitments = fixed
        .comm_rs
        .iter()
        .map(|comm_r| {
            PedersenDomain(
                bytes_into_fr::<Bls12>(comm_r)
                    .expects("could not could not map comm_r to Fr")
                    .into_repr(),
            )
        })
        .collect::<Vec<PedersenDomain>>();

    let public_inputs = vdf_post::PublicInputs::<PedersenDomain> {
        commitments,
        challenge_seed: PedersenDomain::try_from_bytes(&safe_challenge_seed)?,
        faults: fixed.faults.clone(),
    };

    let verifying_key = get_post_verifying_key(fixed.post_config)?;

    let num_post_proof_bytes =
        SINGLE_PARTITION_PROOF_LEN * usize::from(PoStProofPartitions::from(fixed.post_config));

    let proof = MultiProof::new_from_reader(
        Some(usize::from(PoStProofPartitions::from(fixed.post_config))),
        &fixed.proof[0..num_post_proof_bytes],
        &verifying_key,
    )?;

    let is_valid = VDFPostCompound::verify(
        &compound_public_params,
        &public_inputs,
        &proof,
        &NoRequirements,
    )?;

    // Since callers may rely on previous mocked success, just pretend verification succeeded, for now.
    Ok(VerifyPoStFixedSectorsCountOutput { is_valid })
}

fn make_merkle_tree<T: Into<PathBuf> + AsRef<Path>>(
    sealed_path: T,
    bytes: PaddedBytesAmount,
) -> storage_proofs::error::Result<Tree> {
    let mut f_in = File::open(sealed_path.into())?;
    let mut data = Vec::new();
    f_in.read_to_end(&mut data)?;

    public_params(bytes, 1).graph.merkle_tree(&data)
}

fn commitment_from_fr<E: Engine>(fr: E::Fr) -> Commitment {
    let mut commitment = [0; 32];
    for (i, b) in fr_into_bytes::<E>(&fr).iter().enumerate() {
        commitment[i] = *b;
    }
    commitment
}

fn pad_safe_fr(unpadded: &FrSafe) -> Fr32Ary {
    let mut res = [0; 32];
    res[0..31].copy_from_slice(unpadded);
    res
}

#[cfg(test)]
mod tests {
    use std::fs::create_dir_all;
    use std::fs::File;
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;
    use std::thread;

    use rand::{thread_rng, Rng};
    use tempfile::NamedTempFile;

    use sector_base::api::disk_backed_storage::new_sector_store;
    use sector_base::api::disk_backed_storage::TEST_SECTOR_SIZE;
    use sector_base::api::sector_class::SectorClass;
    use sector_base::api::sector_size::SectorSize;
    use sector_base::api::sector_store::SectorStore;

    use super::*;

    const TEST_CLASS: SectorClass = SectorClass(
        SectorSize(TEST_SECTOR_SIZE),
        PoRepProofPartitions(2),
        PoStProofPartitions(1),
    );

    struct Harness {
        prover_id: FrSafe,
        seal_output: SealOutput,
        sealed_access: String,
        sector_id: FrSafe,
        store: Box<SectorStore>,
        unseal_access: String,
        written_contents: Vec<Vec<u8>>,
    }

    #[derive(Debug, Clone, Copy)]
    enum BytesAmount<'a> {
        Max,
        Offset(u64),
        Exact(&'a [u8]),
    }

    fn create_harness(sector_class: SectorClass, bytes_amts: &[BytesAmount]) -> Harness {
        let store = create_sector_store(sector_class);
        let mgr = store.manager();
        let cfg = store.sector_config();
        let max: u64 = store.sector_config().max_unsealed_bytes_per_sector().into();

        let staged_access = mgr
            .new_staging_sector_access()
            .expect("could not create staging access");

        let sealed_access = mgr
            .new_sealed_sector_access()
            .expect("could not create sealed access");

        let unseal_access = mgr
            .new_sealed_sector_access()
            .expect("could not create unseal access");

        let prover_id = [2; 31];
        let sector_id = [0; 31];

        let mut written_contents: Vec<Vec<u8>> = Default::default();
        for bytes_amt in bytes_amts {
            let contents = match bytes_amt {
                BytesAmount::Exact(bs) => bs.to_vec(),
                BytesAmount::Max => make_random_bytes(max),
                BytesAmount::Offset(m) => make_random_bytes(max - m),
            };

            // write contents to temp file and return mutable handle
            let mut file = {
                let mut file = NamedTempFile::new().expects("could not create named temp file");
                let _ = file.write_all(&contents);
                let _ = file
                    .seek(SeekFrom::Start(0))
                    .expects("failed to seek to beginning of file");
                file
            };

            assert_eq!(
                contents.len(),
                usize::from(
                    mgr.write_and_preprocess(&staged_access, &mut file)
                        .expect("failed to write and preprocess")
                )
            );

            written_contents.push(contents);
        }

        let seal_output = seal(
            PoRepConfig::from(sector_class),
            &staged_access,
            &sealed_access,
            &prover_id,
            &sector_id,
        )
        .expect("failed to seal");

        let SealOutput {
            comm_r,
            comm_d,
            comm_r_star,
            proof,
        } = seal_output.clone();

        // valid commitments
        {
            let is_valid = verify_seal(
                PoRepConfig::from(sector_class),
                comm_r,
                comm_d,
                comm_r_star,
                &prover_id,
                &sector_id,
                &proof,
            )
            .expect("failed to run verify_seal");

            assert!(
                is_valid,
                "verification of valid proof failed for sector_class={:?}, bytes_amts={:?}",
                sector_class, bytes_amts
            );
        }

        // unseal the whole thing
        assert_eq!(
            u64::from(UnpaddedBytesAmount::from(PoRepConfig::from(sector_class))),
            u64::from(
                get_unsealed_range(
                    PoRepConfig::from(sector_class),
                    &sealed_access,
                    &unseal_access,
                    &prover_id,
                    &sector_id,
                    UnpaddedByteIndex(0),
                    cfg.max_unsealed_bytes_per_sector(),
                )
                .expect("failed to unseal")
            )
        );

        Harness {
            prover_id,
            seal_output,
            sealed_access,
            sector_id,
            store,
            unseal_access,
            written_contents,
        }
    }

    fn create_sector_store(sector_class: SectorClass) -> Box<SectorStore> {
        let staging_path = tempfile::tempdir().unwrap().path().to_owned();
        let sealed_path = tempfile::tempdir().unwrap().path().to_owned();

        create_dir_all(&staging_path).expect("failed to create staging dir");
        create_dir_all(&sealed_path).expect("failed to create sealed dir");

        Box::new(new_sector_store(
            sector_class,
            sealed_path.to_str().unwrap().to_owned(),
            staging_path.to_str().unwrap().to_owned(),
        ))
    }

    fn make_random_bytes(num_bytes_to_make: u64) -> Vec<u8> {
        let mut rng = thread_rng();
        (0..num_bytes_to_make).map(|_| rng.gen()).collect()
    }

    fn seal_verify_aux(sector_class: SectorClass, bytes_amt: BytesAmount) {
        let h = create_harness(sector_class, &vec![bytes_amt]);

        // invalid commitments
        {
            let is_valid = verify_seal(
                h.store.proofs_config().porep_config(),
                h.seal_output.comm_d,
                h.seal_output.comm_r_star,
                h.seal_output.comm_r,
                &h.prover_id,
                &h.sector_id,
                &h.seal_output.proof,
            )
            .expect("failed to run verify_seal");

            // This should always fail, because we've rotated the commitments in
            // the call. Note that comm_d is passed for comm_r and comm_r_star
            // for comm_d.
            assert!(!is_valid, "proof should not be valid");
        }
    }

    fn post_verify_aux(sector_class: SectorClass, bytes_amt: BytesAmount) {
        let mut rng = thread_rng();
        let h = create_harness(sector_class, &vec![bytes_amt]);
        let seal_output = h.seal_output;

        let comm_r = seal_output.comm_r;
        let comm_rs = vec![comm_r, comm_r];
        let challenge_seed = rng.gen();

        let post_output = generate_post(
            h.store.proofs_config().post_config(),
            challenge_seed,
            vec![
                (Some(h.sealed_access.clone()), comm_r),
                (Some(h.sealed_access.clone()), comm_r),
            ],
        )
        .expect("PoSt generation failed");

        let result = verify_post(
            h.store.proofs_config().post_config(),
            comm_rs,
            challenge_seed,
            post_output.proofs,
            post_output.faults,
        )
        .expect("failed to run verify_post");

        assert!(result.is_valid, "verification of valid proof failed");
    }

    fn seal_unsealed_roundtrip_aux(sector_class: SectorClass, bytes_amt: BytesAmount) {
        let h = create_harness(sector_class, &vec![bytes_amt]);

        let mut file = File::open(&h.unseal_access).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();

        // test A
        {
            let read_unsealed_buf = h
                .store
                .manager()
                .read_raw(&h.unseal_access, 0, UnpaddedBytesAmount(buf.len() as u64))
                .expect("failed to read_raw a");

            assert_eq!(
                &buf, &read_unsealed_buf,
                "test A contents differed for sector_class={:?}, bytes_amt={:?}",
                sector_class, bytes_amt
            );
        }

        // test B
        {
            let read_unsealed_buf = h
                .store
                .manager()
                .read_raw(
                    &h.unseal_access,
                    1,
                    UnpaddedBytesAmount(buf.len() as u64 - 2),
                )
                .expect("failed to read_raw a");

            assert_eq!(
                &buf[1..buf.len() - 1],
                &read_unsealed_buf[..],
                "test B contents differed for sector_class={:?}, bytes_amt={:?}",
                sector_class,
                bytes_amt
            );
        }

        let byte_padding_amount = match bytes_amt {
            BytesAmount::Exact(bs) => {
                let max: u64 = h
                    .store
                    .sector_config()
                    .max_unsealed_bytes_per_sector()
                    .into();
                max - (bs.len() as u64)
            }
            BytesAmount::Max => 0,
            BytesAmount::Offset(m) => m,
        };

        assert_eq!(
            h.written_contents[0].len(),
            buf.len() - (byte_padding_amount as usize),
            "length of original and unsealed contents differed for sector_class={:?}, bytes_amt={:?}",
            sector_class,
            bytes_amt
        );

        assert_eq!(
            h.written_contents[0][..],
            buf[0..h.written_contents[0].len()],
            "original and unsealed contents differed for sector_class={:?}, bytes_amt={:?}",
            sector_class,
            bytes_amt
        );
    }

    fn seal_unsealed_range_roundtrip_aux(sector_class: SectorClass, bytes_amt: BytesAmount) {
        let h = create_harness(sector_class, &vec![bytes_amt]);

        let offset = 5;
        let range_length = h.written_contents[0].len() as u64 - offset;

        assert_eq!(
            range_length,
            u64::from(
                get_unsealed_range(
                    h.store.proofs_config().porep_config(),
                    &PathBuf::from(&h.sealed_access),
                    &PathBuf::from(&h.unseal_access),
                    &h.prover_id,
                    &h.sector_id,
                    UnpaddedByteIndex(offset),
                    UnpaddedBytesAmount(range_length),
                )
                .expect("failed to unseal")
            )
        );

        let mut file = File::open(&h.unseal_access).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();

        assert_eq!(
            h.written_contents[0][(offset as usize)..],
            buf[0..(range_length as usize)],
            "original and unsealed range contents differed for sector_class={:?}, bytes_amt={:?}",
            sector_class,
            bytes_amt
        );
    }

    fn write_and_preprocess_overwrites_unaligned_last_bytes_aux(sector_class: SectorClass) {
        // The minimal reproduction for the bug this regression test checks is to write
        // 32 bytes, then 95 bytes.
        // The bytes must sum to 127, since that is the required unsealed sector size.
        // With suitable bytes (.e.g all 255), the bug always occurs when the first chunk is >= 32.
        // It never occurs when the first chunk is < 32.
        // The root problem was that write_and_preprocess was opening in append mode, so seeking backward
        // to overwrite the last, incomplete byte, was not happening.
        let contents_a = [255; 32];
        let contents_b = [255; 95];

        let h = create_harness(
            sector_class,
            &vec![
                BytesAmount::Exact(&contents_a),
                BytesAmount::Exact(&contents_b),
            ],
        );

        let unseal_access = h
            .store
            .manager()
            .new_sealed_sector_access()
            .expect("could not create unseal access");

        let _ = get_unsealed_range(
            h.store.proofs_config().porep_config(),
            &h.sealed_access,
            &unseal_access,
            &h.prover_id,
            &h.sector_id,
            UnpaddedByteIndex(0),
            UnpaddedBytesAmount((contents_a.len() + contents_b.len()) as u64),
        )
        .expect("failed to unseal");

        let mut file = File::open(&unseal_access).unwrap();
        let mut buf_from_file = Vec::new();
        file.read_to_end(&mut buf_from_file).unwrap();

        assert_eq!(
            contents_a.len() + contents_b.len(),
            buf_from_file.len(),
            "length of original and unsealed contents differed for {:?}",
            sector_class
        );

        assert_eq!(
            contents_a[..],
            buf_from_file[0..contents_a.len()],
            "original and unsealed contents differed for {:?}",
            sector_class
        );

        assert_eq!(
            contents_b[..],
            buf_from_file[contents_a.len()..contents_a.len() + contents_b.len()],
            "original and unsealed contents differed for {:?}",
            sector_class
        );
    }

    /*

    TODO: create a way to run these super-slow-by-design tests manually.

    fn seal_verify_live() {
        seal_verify_aux(ConfiguredStore::Live, 0);
        seal_verify_aux(ConfiguredStore::Live, 5);
    }

    fn seal_unsealed_roundtrip_live() {
        seal_unsealed_roundtrip_aux(ConfiguredStore::Live, 0);
        seal_unsealed_roundtrip_aux(ConfiguredStore::Live, 5);
    }

    fn seal_unsealed_range_roundtrip_live() {
        seal_unsealed_range_roundtrip_aux(ConfiguredStore::Live, 0);
        seal_unsealed_range_roundtrip_aux(ConfiguredStore::Live, 5);
    }

    */

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn seal_verify_test() {
        seal_verify_aux(TEST_CLASS, BytesAmount::Max);
        seal_verify_aux(TEST_CLASS, BytesAmount::Offset(5));
    }

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn seal_unsealed_roundtrip_test() {
        seal_unsealed_roundtrip_aux(TEST_CLASS, BytesAmount::Max);
        seal_unsealed_roundtrip_aux(TEST_CLASS, BytesAmount::Offset(5));
    }

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn seal_unsealed_range_roundtrip_test() {
        seal_unsealed_range_roundtrip_aux(TEST_CLASS, BytesAmount::Max);
        seal_unsealed_range_roundtrip_aux(TEST_CLASS, BytesAmount::Offset(5));
    }

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn write_and_preprocess_overwrites_unaligned_last_bytes() {
        write_and_preprocess_overwrites_unaligned_last_bytes_aux(TEST_CLASS);
    }

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn concurrent_seal_unsealed_range_roundtrip_test() {
        let threads = 5;

        let spawned = (0..threads)
            .map(|_| {
                thread::spawn(|| seal_unsealed_range_roundtrip_aux(TEST_CLASS, BytesAmount::Max))
            })
            .collect::<Vec<_>>();

        for thread in spawned {
            thread.join().expect("test thread panicked");
        }
    }

    #[test]
    #[ignore]
    fn post_verify_test() {
        post_verify_aux(TEST_CLASS, BytesAmount::Max);
    }
}