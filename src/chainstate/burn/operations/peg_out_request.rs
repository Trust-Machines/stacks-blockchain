use clarity::codec::StacksMessageCodec;
use clarity::util::secp256k1::MessageSignature;

use crate::burnchains::BurnchainBlockHeader;
use crate::burnchains::BurnchainTransaction;
use crate::chainstate::burn::Opcodes;
use crate::types::chainstate::StacksAddress;
use crate::types::Address;

use crate::chainstate::burn::operations::Error as OpError;
use crate::chainstate::burn::operations::PegOutRequestOp;

impl PegOutRequestOp {
    pub fn from_tx(
        block_header: &BurnchainBlockHeader,
        tx: &BurnchainTransaction,
    ) -> Result<Self, OpError> {
        todo!();
    }

    fn parse_data(data: &[u8]) -> Result<ParsedData, ParseError> {
        /*
            Wire format:

            0      2  3         11                76
            |------|--|---------|-----------------|
             magic  op   amount      signature

             Note that `data` is missing the first 3 bytes -- the magic and op must
             be stripped before this method is called. At the time of writing,
             this is done in `burnchains::bitcoin::blocks::BitcoinBlockParser::parse_data`.
        */

        if data.len() < 73 {
            // too short
            warn!(
                "PegOutRequestOp payload is malformed ({} bytes, expected {})",
                data.len(),
                73
            );
            return Err(ParseError::MalformedPayload);
        }

        let amount = u64::from_be_bytes(data[0..8].try_into()?);
        let signature = MessageSignature(data[8..73].try_into()?);

        Ok(ParsedData { amount, signature })
    }

    pub fn check(&self) -> Result<(), OpError> {
        todo!();
    }
}

struct ParsedData {
    amount: u64,
    signature: MessageSignature,
}

enum ParseError {
    MalformedPayload,
    SliceConversion,
}

impl From<ParseError> for OpError {
    fn from(_: ParseError) -> Self {
        Self::ParseError
    }
}

impl From<std::array::TryFromSliceError> for ParseError {
    fn from(_: std::array::TryFromSliceError) -> Self {
        Self::SliceConversion
    }
}

#[cfg(test)]
mod tests {}