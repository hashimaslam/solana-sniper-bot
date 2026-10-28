//! Parser for SIMD-0385 "v1" transactions.
//!
//! Mainnet now carries v1 transactions (version byte `129`), and pump.fun
//! launches already use them. `solana-sdk` 2.x can't deserialize the format,
//! so this module reads the wire layout directly and maps it onto a
//! `VersionedMessage::V0` with no address lookup tables. That is lossless for
//! everything the decoder needs: v1 has no lookup tables, the account key
//! order is unchanged, and instructions are the same (program index,
//! account indexes, data). The transaction config values (priority fee,
//! CU limit, ...) are dropped.
//!
//! Wire layout:
//! ```text
//! VersionByte u8 (=129) | LegacyHeader (u8,u8,u8) | ConfigMask u32
//! | LifetimeSpecifier [u8;32] | NumInstructions u8 | NumAddresses u8
//! | Addresses [[u8;32]] | ConfigValues [[u8;4]; popcount(mask)]
//! | InstructionHeaders [(u8 program, u8 n_accounts, u16 n_data)]
//! | InstructionPayloads [accounts ++ data] | Signatures [[u8;64]; num_required_signatures]
//! ```

use solana_sdk::{
    hash::Hash,
    instruction::CompiledInstruction,
    message::{v0, MessageHeader, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};

use crate::error::{Error, Result};

/// Version byte identifying a v1 transaction.
pub const V1_VERSION_BYTE: u8 = 129;

/// `true` if the serialized transaction is in the v1 format.
pub fn is_v1(bytes: &[u8]) -> bool {
    bytes.first() == Some(&V1_VERSION_BYTE)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| {
                Error::Decode(format!(
                    "v1 tx truncated: need {} bytes at offset {}, have {}",
                    n,
                    self.pos,
                    self.buf.len()
                ))
            })?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }
}

/// Parse a serialized v1 transaction into an equivalent `VersionedTransaction`.
pub fn parse_v1(bytes: &[u8]) -> Result<VersionedTransaction> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let version = r.u8()?;
    if version != V1_VERSION_BYTE {
        return Err(Error::Decode(format!("not a v1 transaction (version byte {})", version)));
    }
    let header = MessageHeader {
        num_required_signatures: r.u8()?,
        num_readonly_signed_accounts: r.u8()?,
        num_readonly_unsigned_accounts: r.u8()?,
    };
    if header.num_readonly_signed_accounts >= header.num_required_signatures {
        return Err(Error::Decode("v1 tx: invalid header".into()));
    }
    let config_mask = r.u32()?;
    let recent_blockhash = Hash::new_from_array(r.take(32)?.try_into().expect("32 bytes"));
    let num_instructions = r.u8()? as usize;
    let num_addresses = r.u8()? as usize;

    let account_keys = (0..num_addresses)
        .map(|_| Ok(Pubkey::new_from_array(r.take(32)?.try_into().expect("32 bytes"))))
        .collect::<Result<Vec<_>>>()?;

    // Config values: 4 bytes per set bit. We don't need them.
    r.take(config_mask.count_ones() as usize * 4)?;

    let headers = (0..num_instructions)
        .map(|_| Ok((r.u8()?, r.u8()? as usize, r.u16()? as usize)))
        .collect::<Result<Vec<_>>>()?;

    let mut instructions = Vec::with_capacity(num_instructions);
    for (program_id_index, n_accounts, n_data) in headers {
        let accounts = r.take(n_accounts)?.to_vec();
        if (program_id_index as usize) >= num_addresses
            || accounts.iter().any(|a| (*a as usize) >= num_addresses)
        {
            return Err(Error::Decode("v1 tx: account index out of range".into()));
        }
        let data = r.take(n_data)?.to_vec();
        instructions.push(CompiledInstruction {
            program_id_index,
            accounts,
            data,
        });
    }

    let signatures = (0..header.num_required_signatures)
        .map(|_| {
            Signature::try_from(r.take(64)?)
                .map_err(|_| Error::Decode("v1 tx: bad signature".into()))
        })
        .collect::<Result<Vec<_>>>()?;

    if r.pos != bytes.len() {
        return Err(Error::Decode(format!(
            "v1 tx: {} trailing bytes",
            bytes.len() - r.pos
        )));
    }

    Ok(VersionedTransaction {
        signatures,
        message: VersionedMessage::V0(v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups: vec![],
        }),
    })
}

/// Deserialize any transaction: legacy, v0 (bincode) or v1.
pub fn deserialize_any(bytes: &[u8]) -> Result<VersionedTransaction> {
    if is_v1(bytes) {
        parse_v1(bytes)
    } else {
        bincode::deserialize(bytes)
            .map_err(|e| Error::Decode(format!("failed to deserialize transaction: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::str::FromStr;

    /// Real mainnet pump.fun launch (create_v2 + buy_v2) sent as a v1 tx.
    /// Signature 4PenvsM4SVVC3NDKuc4LAHg1xnbinjRRZqrFzyEK7xknLpRWrLt8T5XACV5m3X5JVevBqMtqCbAoxUPVEmFZHH5X
    fn fixture() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(include_str!("../tests/fixtures/create_v2_tx_v1.b64").trim())
            .unwrap()
    }

    #[test]
    fn parses_real_mainnet_v1_transaction() {
        let bytes = fixture();
        assert!(is_v1(&bytes));
        assert!(bincode::deserialize::<VersionedTransaction>(&bytes).is_err(), "sdk can't read v1");

        let tx = deserialize_any(&bytes).unwrap();
        assert_eq!(
            tx.signatures[0],
            Signature::from_str("4PenvsM4SVVC3NDKuc4LAHg1xnbinjRRZqrFzyEK7xknLpRWrLt8T5XACV5m3X5JVevBqMtqCbAoxUPVEmFZHH5X").unwrap()
        );
        let keys = tx.message.static_account_keys();
        let pump = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();
        let pump_ixs: Vec<_> = tx
            .message
            .instructions()
            .iter()
            .filter(|ix| keys[ix.program_id_index as usize] == pump)
            .collect();
        assert_eq!(pump_ixs.len(), 2);
        // create_v2: 16 accounts, mint first.
        assert_eq!(&pump_ixs[0].data[..8], &[214, 144, 76, 236, 95, 139, 49, 180]);
        assert_eq!(pump_ixs[0].accounts.len(), 16);
        assert_eq!(
            keys[pump_ixs[0].accounts[0] as usize],
            Pubkey::from_str("BpDR9CZtZKYMu1a9L8yBfdXVJx9NcbZjkaVYSRq8pump").unwrap()
        );
        // buy_v2: 27 accounts.
        assert_eq!(pump_ixs[1].accounts.len(), 27);
        // The signature really is over the message: verify it.
        assert!(tx.signatures[0].verify(keys[0].as_ref(), &bytes[..bytes.len() - 64 * tx.signatures.len()]));
    }

    #[test]
    fn rejects_truncated_and_trailing() {
        let bytes = fixture();
        for cut in [1, 10, 100, bytes.len() - 1] {
            assert!(parse_v1(&bytes[..cut]).is_err(), "cut {}", cut);
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(parse_v1(&extra).is_err());
    }

    #[test]
    fn legacy_still_goes_through_bincode() {
        use solana_sdk::{message::Message, signature::Keypair, signer::Signer, transaction::Transaction};
        let kp = Keypair::new();
        let mut t = Transaction::new_unsigned(Message::new(&[], Some(&kp.pubkey())));
        t.sign(&[&kp], Hash::new_unique());
        let vtx: VersionedTransaction = t.into();
        let back = deserialize_any(&bincode::serialize(&vtx).unwrap()).unwrap();
        assert_eq!(back, vtx);
    }
}
