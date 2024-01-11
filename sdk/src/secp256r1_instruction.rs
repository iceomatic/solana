//! Instructions for the [secp256r1 native program][np].
//!
//! [np]: https://docs.solana.com/developing/runtime-facilities/programs#secp256r1-program

#![cfg(feature = "full")]

use {
    crate::{feature_set::FeatureSet, instruction::Instruction, precompiles::PrecompileError},
    bytemuck::{bytes_of, Pod, Zeroable},
    p256::{
        ecdsa::{
            //Signature,
            signature::Signer,
            SigningKey, VerifyingKey,
        }
    },
    openssl::bn::{BigNum, BigNumContext},
    openssl::ec::{EcGroup, EcKey, EcPoint},
    openssl::pkey::PKey,
    openssl::nid::Nid,
    openssl::sign::Verifier,
};

pub const COMPRESSED_PUBKEY_SERIALIZED_SIZE: usize = 33;
pub const SIGNATURE_SERIALIZED_SIZE: usize = 64;
pub const SIGNATURE_OFFSETS_SERIALIZED_SIZE: usize = 14;
// bytemuck requires structures to be aligned
pub const SIGNATURE_OFFSETS_START: usize = 2;
pub const DATA_START: usize = SIGNATURE_OFFSETS_SERIALIZED_SIZE + SIGNATURE_OFFSETS_START;

#[derive(Default, Debug, Copy, Clone, Zeroable, Pod, Eq, PartialEq)]
#[repr(C)]
pub struct Secp256r1SignatureOffsets {
    signature_offset: u16, // offset to compact secp256r1 signature of 64 bytes
    signature_instruction_index: u16, // instruction index to find signature
    public_key_offset: u16, // offset to compressed public key of 33 bytes
    public_key_instruction_index: u16, // instruction index to find public key
    message_data_offset: u16, // offset to start of message data
    message_data_size: u16, // size of message data
    message_instruction_index: u16, // index of instruction data to get message data
}

pub fn new_secp256r1_instruction(signer: &SigningKey, message: &[u8]) -> Instruction {
    let signature = signer.sign(message);
    let signature = signature.normalize_s().unwrap_or(signature).to_vec();
    let pubkey = VerifyingKey::from(signer).to_encoded_point(true).to_bytes();

    assert_eq!(pubkey.len(), COMPRESSED_PUBKEY_SERIALIZED_SIZE);
    assert_eq!(signature.len(), SIGNATURE_SERIALIZED_SIZE);

    let mut instruction_data = Vec::with_capacity(
        DATA_START
            .saturating_add(SIGNATURE_SERIALIZED_SIZE)
            .saturating_add(COMPRESSED_PUBKEY_SERIALIZED_SIZE)
            .saturating_add(message.len()),
    );

    let num_signatures: u8 = 1;
    let public_key_offset = DATA_START;
    let signature_offset = public_key_offset.saturating_add(COMPRESSED_PUBKEY_SERIALIZED_SIZE);
    let message_data_offset = signature_offset.saturating_add(SIGNATURE_SERIALIZED_SIZE);

    // add padding byte so that offset structure is aligned
    instruction_data.extend_from_slice(bytes_of(&[num_signatures, 0]));

    let offsets = Secp256r1SignatureOffsets {
        signature_offset: signature_offset as u16,
        signature_instruction_index: u16::MAX,
        public_key_offset: public_key_offset as u16,
        public_key_instruction_index: u16::MAX,
        message_data_offset: message_data_offset as u16,
        message_data_size: message.len() as u16,
        message_instruction_index: u16::MAX,
    };

    instruction_data.extend_from_slice(bytes_of(&offsets));

    debug_assert_eq!(instruction_data.len(), public_key_offset);

    instruction_data.extend_from_slice(&pubkey);

    debug_assert_eq!(instruction_data.len(), signature_offset);

    instruction_data.extend_from_slice(&signature);

    debug_assert_eq!(instruction_data.len(), message_data_offset);

    instruction_data.extend_from_slice(message);

    Instruction {
        program_id: solana_sdk::secp256r1_program::id(),
        accounts: vec![],
        data: instruction_data,
    }
}

pub fn verify(
    data: &[u8],
    instruction_datas: &[&[u8]],
    _feature_set: &FeatureSet,
) -> Result<(), PrecompileError> {
    if data.len() < SIGNATURE_OFFSETS_START {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let num_signatures = data[0] as usize;
    if num_signatures == 0 && data.len() > SIGNATURE_OFFSETS_START {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    let expected_data_size = num_signatures
        .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
        .saturating_add(SIGNATURE_OFFSETS_START);
    // We do not check or use the byte at data[1]
    if data.len() < expected_data_size {
        return Err(PrecompileError::InvalidInstructionDataSize);
    }
    for i in 0..num_signatures {
        let start = i
            .saturating_mul(SIGNATURE_OFFSETS_SERIALIZED_SIZE)
            .saturating_add(SIGNATURE_OFFSETS_START);
        let end = start.saturating_add(SIGNATURE_OFFSETS_SERIALIZED_SIZE);

        // bytemuck wants structures aligned
        let offsets: &Secp256r1SignatureOffsets = bytemuck::try_from_bytes(&data[start..end])
            .map_err(|_| PrecompileError::InvalidDataOffsets)?;

        // Parse out signature
        let signature = get_data_slice(
            data,
            instruction_datas,
            offsets.signature_instruction_index,
            offsets.signature_offset,
            SIGNATURE_SERIALIZED_SIZE,
        )?;

        // Parse out pubkey
        let pubkey = get_data_slice(
            data,
            instruction_datas,
            offsets.public_key_instruction_index,
            offsets.public_key_offset,
            COMPRESSED_PUBKEY_SERIALIZED_SIZE,
        )?;

        // Parse out message
        let message = get_data_slice(
            data,
            instruction_datas,
            offsets.message_instruction_index,
            offsets.message_data_offset,
            offsets.message_data_size as usize,
        )?;

        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).map_err(|_| PrecompileError::InvalidSignature)?;
        let mut ctx = BigNumContext::new().map_err(|_| PrecompileError::InvalidSignature)?;
        let mut order = BigNum::new().map_err(|_| PrecompileError::InvalidSignature)?;

        group.order(&mut order, &mut ctx).map_err(|_| PrecompileError::InvalidSignature)?;

        // Calculate half_order = order / 2
        let mut half_order = BigNum::new().map_err(|_| PrecompileError::InvalidSignature)?;
        half_order.rshift1(&order).map_err(|_| PrecompileError::InvalidSignature)?;

        // Calculate n_minus_one = order - 1
        let one = BigNum::from_u32(1).map_err(|_| PrecompileError::InvalidSignature)?;
        let mut n_minus_one = BigNum::new().map_err(|_| PrecompileError::InvalidSignature)?;
        n_minus_one.checked_sub(&order, &one).map_err(|_| PrecompileError::InvalidSignature)?;

        let r_bignum = BigNum::from_slice(&signature[..32]).map_err(|_| PrecompileError::InvalidSignature)?;
        let s_bignum = BigNum::from_slice(&signature[32..]).map_err(|_| PrecompileError::InvalidSignature)?;
    
        // Since OpenSSL doesnt know what curve this signature is for, we have
        // to check that r and s are within the order of the curve.
        let within_order_minus_one = r_bignum > one && r_bignum < n_minus_one && s_bignum > one && s_bignum < n_minus_one;
        if !within_order_minus_one {
            return Err(PrecompileError::InvalidSignature);
        }
        // Create an ECDSA signature object from the ASN.1 integers
        let ecdsa_sig = openssl::ecdsa::EcdsaSig::from_private_components(r_bignum, s_bignum).map_err(|_| PrecompileError::InvalidSignature)?;
        //println!("Sig: {:?}", ecdsa_sig.to_der().map_err(|_| PrecompileError::InvalidSignature)?);
        let ecdsa_sig_der = ecdsa_sig.to_der().map_err(|_| PrecompileError::InvalidSignature)?;
    

        // Enforce Low-S
        if ecdsa_sig.s() > &half_order {
            return Err(PrecompileError::InvalidSignature);
        }

        let public_key_point = EcPoint::from_bytes(&group, pubkey, &mut ctx).map_err(|_| PrecompileError::InvalidPublicKey)?;
        let public_key = EcKey::from_public_key(&group, &public_key_point).map_err(|_| PrecompileError::InvalidPublicKey)?;
        let pkey = PKey::from_ec_key(public_key).map_err(|_| PrecompileError::InvalidPublicKey)?;

        let mut verifier = Verifier::new(openssl::hash::MessageDigest::sha256(), &pkey).map_err(|_| PrecompileError::InvalidSignature)?;
        verifier.update(message).map_err(|_| PrecompileError::InvalidSignature)?; 

        let result = verifier.verify(&ecdsa_sig_der).map_err(|_| PrecompileError::InvalidSignature)?;
        if !result {
            return Err(PrecompileError::InvalidSignature);
        }
    }
    Ok(())
}

fn get_data_slice<'a>(
    data: &'a [u8],
    instruction_datas: &'a [&[u8]],
    instruction_index: u16,
    offset_start: u16,
    size: usize,
) -> Result<&'a [u8], PrecompileError> {
    let instruction = if instruction_index == u16::MAX {
        data
    } else {
        let signature_index = instruction_index as usize;
        if signature_index >= instruction_datas.len() {
            return Err(PrecompileError::InvalidDataOffsets);
        }
        instruction_datas[signature_index]
    };

    let start = offset_start as usize;
    let end = start.saturating_add(size);
    if end > instruction.len() {
        return Err(PrecompileError::InvalidDataOffsets);
    }

    Ok(&instruction[start..end])
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        crate::{
            feature_set::FeatureSet,
            hash::Hash,
            secp256r1_instruction::new_secp256r1_instruction,
            signature::{Keypair, Signer},
            transaction::Transaction,
        },
        rand0_7::{thread_rng, Rng},
    };

    fn test_case(
        num_signatures: u16,
        offsets: &Secp256r1SignatureOffsets,
    ) -> Result<(), PrecompileError> {
        assert_eq!(
            bytemuck::bytes_of(offsets).len(),
            SIGNATURE_OFFSETS_SERIALIZED_SIZE
        );

        let mut instruction_data = vec![0u8; DATA_START];
        instruction_data[0..SIGNATURE_OFFSETS_START].copy_from_slice(bytes_of(&num_signatures));
        instruction_data[SIGNATURE_OFFSETS_START..DATA_START].copy_from_slice(bytes_of(offsets));
        verify(
            &instruction_data,
            &[&[0u8; 100]],
            &FeatureSet::all_enabled(),
        )
    }

    #[test]
    fn test_invalid_offsets() {
        solana_logger::setup();

        let mut instruction_data = vec![0u8; DATA_START];
        let offsets = Secp256r1SignatureOffsets::default();
        instruction_data[0..SIGNATURE_OFFSETS_START].copy_from_slice(bytes_of(&1u16));
        instruction_data[SIGNATURE_OFFSETS_START..DATA_START].copy_from_slice(bytes_of(&offsets));
        instruction_data.truncate(instruction_data.len() - 1);

        assert_eq!(
            verify(
                &instruction_data,
                &[&[0u8; 100]],
                &FeatureSet::all_enabled(),
            ),
            Err(PrecompileError::InvalidInstructionDataSize)
        );

        let offsets = Secp256r1SignatureOffsets {
            signature_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            public_key_instruction_index: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_message_data_offsets() {
        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 99,
            message_data_size: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidSignature)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: 100,
            message_data_size: 1000,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            message_data_offset: std::u16::MAX,
            message_data_size: std::u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_pubkey_offset() {
        let offsets = Secp256r1SignatureOffsets {
            public_key_offset: std::u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            public_key_offset: 100 - COMPRESSED_PUBKEY_SERIALIZED_SIZE as u16 + 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_signature_offset() {
        let offsets = Secp256r1SignatureOffsets {
            signature_offset: std::u16::MAX,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );

        let offsets = Secp256r1SignatureOffsets {
            signature_offset: 100 - SIGNATURE_SERIALIZED_SIZE as u16 + 1,
            ..Secp256r1SignatureOffsets::default()
        };
        assert_eq!(
            test_case(1, &offsets),
            Err(PrecompileError::InvalidDataOffsets)
        );
    }

    #[test]
    fn test_secp256r1() {
        solana_logger::setup();
        let privkey = p256::ecdsa::SigningKey::random(rand::thread_rng());
        let message_arr = b"hello";
        let mut instruction = new_secp256r1_instruction(&privkey, message_arr);
        let mint_keypair = Keypair::new();
        let feature_set = FeatureSet::all_enabled();

        let tx = Transaction::new_signed_with_payer(
            &[instruction.clone()],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            Hash::default(),
        );

        assert!(tx.verify_precompiles(&feature_set).is_ok());

        let index = loop {
            let index = thread_rng().gen_range(0, instruction.data.len());
            // byte 1 is not used, so this would not cause the verify to fail
            if index != 1 {
                break index;
            }
        };

        instruction.data[index] = instruction.data[index].wrapping_add(12);
        let tx = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            Hash::default(),
        );
        assert!(tx.verify_precompiles(&feature_set).is_err());
    }
}