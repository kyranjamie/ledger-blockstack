use core::fmt::{self, Write};
use nom::{
    branch::permutation,
    bytes::complete::take,
    combinator::{iterator, map_parser},
    error::ErrorKind,
    number::complete::{be_u32, be_u64, le_u8},
};

use arrayvec::ArrayVec;

use crate::parser::parser_common::{
    u8_with_limits, AssetInfo, AssetInfoId, ClarityName, ContractName, ParserError, PrincipalData,
    StacksAddress, StacksString, StandardPrincipal, C32_ENCODED_ADDRS_LENGTH, HASH160_LEN,
    MAX_STACKS_STRING_LEN, MAX_STRING_LEN, NUM_SUPPORTED_POST_CONDITIONS, STX_DECIMALS,
};

use crate::parser::c32;

use crate::parser::ffi::fp_uint64_to_str;
use crate::parser::value::{Value, BIG_INT_SIZE};
use crate::{check_canary, is_expert_mode, zxformat};

pub const MAX_NUM_ARGS: u32 = 10;

#[repr(u8)]
#[derive(Debug, Clone, PartialEq)]
pub enum TokenTranferPrincipal {
    Standard = 0x05,
    Contract = 0x06,
}

impl TokenTranferPrincipal {
    fn from_u8(v: u8) -> Result<Self, ParserError> {
        match v {
            5 => Ok(Self::Standard),
            6 => Ok(Self::Contract),
            _ => Err(ParserError::parser_invalid_token_transfer_principal),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq)]
pub struct StxTokenTransfer<'a>(&'a [u8]);

impl<'a> StxTokenTransfer<'a> {
    #[inline(never)]
    fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let id = le_u8(bytes)?;
        let (raw, _) = match TokenTranferPrincipal::from_u8(id.1)? {
            TokenTranferPrincipal::Standard => PrincipalData::standard_from_bytes(id.0)?,
            TokenTranferPrincipal::Contract => PrincipalData::contract_principal_from_bytes(id.0)?,
        };
        // Besides principal we take 34-bytes being the MEMO message + 8-bytes amount of stx
        let len = bytes.len() - raw.len() + 34 + 8;
        let (raw, data) = take(len)(bytes)?;
        Ok((raw, Self(data)))
    }

    pub fn memo(&self) -> &[u8] {
        let at = self.0.len() - 34;
        &self.0[at..]
    }

    pub fn amount(&self) -> Result<u64, ParserError> {
        let at = self.0.len() - 34 - 8;
        be_u64::<'a, ParserError>(&self.0[at..])
            .map(|res| res.1)
            .map_err(|_| ParserError::parser_unexpected_buffer_end)
    }

    pub fn raw_address(&self) -> &[u8] {
        // Skips the principal-id and hash_mode
        &self.0[2..22]
    }

    pub fn encoded_address(
        &self,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        // Skips the principal-id at [0] and uses hash_mode and the follow 20-bytes
        let version = self.0[1];
        c32::c32_address(version, &self.0[2..22])
    }

    pub fn amount_stx(&self) -> Result<ArrayVec<[u8; zxformat::MAX_STR_BUFF_LEN]>, ParserError> {
        let mut output = ArrayVec::from([0u8; zxformat::MAX_STR_BUFF_LEN]);
        let amount = self.amount()?;
        let len = zxformat::u64_to_str(output.as_mut(), amount)? as usize;
        unsafe {
            output.set_len(len);
        }
        check_canary!();
        Ok(output)
    }

    fn get_token_transfer_items(
        &self,
        display_idx: u8,
        out_key: &mut [u8],
        out_value: &mut [u8],
        page_idx: u8,
    ) -> Result<u8, ParserError> {
        let mut writer_key = zxformat::Writer::new(out_key);

        match display_idx {
            // Fomatting the amount in stx
            0 => {
                writer_key
                    .write_str("Amount uSTX")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                let amount = self.amount_stx()?;
                check_canary!();
                zxformat::pageString(out_value, amount.as_ref(), page_idx)
            }
            // Recipient address
            1 => {
                writer_key
                    .write_str("To")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                let recipient = self.encoded_address()?;
                check_canary!();
                zxformat::pageString(out_value, recipient.as_ref(), page_idx)
            }
            2 => {
                writer_key
                    .write_str("Memo")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                check_canary!();
                zxformat::pageString(out_value, self.memo(), page_idx)
            }
            _ => Err(ParserError::parser_display_idx_out_of_range),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq)]
pub struct Arguments<'a>(&'a [u8]);

impl<'a> Arguments<'a> {
    #[inline(never)]
    fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let len = be_u32(bytes)?;
        if len.1 > MAX_NUM_ARGS && !is_expert_mode() {
            return Err(nom::Err::Error(
                ParserError::parser_invalid_transaction_payload,
            ));
        }
        // here we take the leftover bytes after reading the args len
        // because they are meant to be the argument values and nothing else
        let (raw, args) = take(bytes.len())(bytes)?;
        check_canary!();
        Ok((raw, Self(args)))
    }

    pub fn num_args(&self) -> Result<u32, ParserError> {
        be_u32::<'a, ParserError>(self.0)
            .map(|res| res.1)
            .map_err(|_| ParserError::parser_unexpected_error)
    }
}

/// A transaction that calls into a smart contract
#[repr(C)]
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionContractCall<'a>(&'a [u8]);

impl<'a> TransactionContractCall<'a> {
    #[inline(never)]
    fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let (raw, _) = StacksAddress::from_bytes(bytes)?;
        let (raw2, _) = permutation((ContractName::from_bytes, ClarityName::from_bytes))(raw)?;
        let (leftover, _) = Arguments::from_bytes(raw2)?;
        let len = bytes.len() - leftover.len();
        let (_, data) = take(len)(bytes)?;
        check_canary!();
        Ok((leftover, Self(data)))
    }

    pub fn contract_name(&self) -> Result<&[u8], ParserError> {
        let at = HASH160_LEN + 1;
        ContractName::from_bytes(&self.0[at..])
            .map(|res| (res.1).0)
            .map_err(|_| ParserError::parser_invalid_contract_name)
    }

    pub fn function_name(&self) -> Result<&[u8], ParserError> {
        ContractName::from_bytes(&self.0[(HASH160_LEN + 1)..])
            .and_then(|b| ClarityName::from_bytes(b.0))
            .map(|res| (res.1).0)
            .map_err(|_| ParserError::parser_unexpected_error)
    }

    pub fn function_args(&self) -> Result<Arguments, ParserError> {
        ContractName::from_bytes(&self.0[(HASH160_LEN + 1)..])
            .and_then(|b| ClarityName::from_bytes(b.0))
            .and_then(|c| Arguments::from_bytes(c.0))
            .map(|res| res.1)
            .map_err(|_| ParserError::parser_invalid_argument_id)
    }

    pub fn num_args(&self) -> Result<u32, ParserError> {
        self.function_args().and_then(|args| args.num_args())
    }

    #[inline(never)]
    pub fn contract_address(
        &self,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        let version = self.0[0];
        c32::c32_address(version, &self.0[1..21])
    }

    pub fn num_items(&self) -> u8 {
        // contract-address, contract-name, function-name
        3
    }

    fn get_contract_call_items(
        &self,
        display_idx: u8,
        out_key: &mut [u8],
        out_value: &mut [u8],
        page_idx: u8,
    ) -> Result<u8, ParserError> {
        let mut writer_key = zxformat::Writer::new(out_key);

        match display_idx {
            // Contract-address
            0 => {
                writer_key
                    .write_str("Contract address")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                let address = self.contract_address()?;
                check_canary!();
                zxformat::pageString(out_value, &address[..address.len()], page_idx)
            }
            // Contract.name
            1 => {
                writer_key
                    .write_str("Contract name")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                let name = self.contract_name()?;
                check_canary!();
                zxformat::pageString(out_value, name, page_idx)
            }
            // Function-name
            2 => {
                writer_key
                    .write_str("Function name")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                let name = self.function_name()?;
                check_canary!();
                zxformat::pageString(out_value, name, page_idx)
            }
            _ => Err(ParserError::parser_value_out_of_range),
        }
    }
}

/// A transaction that instantiates a smart contract
#[repr(C)]
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionSmartContract<'a>(&'a [u8]);

impl<'a> TransactionSmartContract<'a> {
    #[inline(never)]
    fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        check_canary!();
        // we take "ownership" of bytes here because
        // it should only contain the contract information and body
        Ok((Default::default(), Self(bytes)))
    }

    pub fn contract_name(&self) -> Result<&[u8], ParserError> {
        ContractName::from_bytes(self.0)
            .map(|res| (res.1).0)
            .map_err(|_| ParserError::parser_invalid_contract_name)
    }

    #[inline(never)]
    fn get_contract_items(
        &self,
        display_idx: u8,
        out_key: &mut [u8],
        out_value: &mut [u8],
        page_idx: u8,
    ) -> Result<u8, ParserError> {
        let mut writer_key = zxformat::Writer::new(out_key);

        match display_idx {
            0 => {
                writer_key
                    .write_str("Contract Name")
                    .map_err(|_| ParserError::parser_unexpected_buffer_end)?;
                check_canary!();
                let name = self.contract_name()?;
                zxformat::pageString(out_value, name, page_idx)
            }
            _ => Err(ParserError::parser_value_out_of_range),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum TransactionPayloadId {
    TokenTransfer = 0,
    SmartContract = 1,
    ContractCall = 2,
}

impl TransactionPayloadId {
    fn from_u8(v: u8) -> Result<Self, ParserError> {
        match v {
            0 => Ok(Self::TokenTransfer),
            1 => Ok(Self::SmartContract),
            2 => Ok(Self::ContractCall),
            _ => Err(ParserError::parser_invalid_transaction_payload),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionPayload<'a> {
    TokenTransfer(StxTokenTransfer<'a>),
    SmartContract(TransactionSmartContract<'a>),
    ContractCall(TransactionContractCall<'a>),
}

impl<'a> TransactionPayload<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let id = le_u8(bytes)?;
        let res = match TransactionPayloadId::from_u8(id.1)? {
            TransactionPayloadId::TokenTransfer => {
                let token = StxTokenTransfer::from_bytes(id.0)?;
                (token.0, Self::TokenTransfer(token.1))
            }
            TransactionPayloadId::SmartContract => {
                let contract = TransactionSmartContract::from_bytes(id.0)?;
                (contract.0, Self::SmartContract(contract.1))
            }
            TransactionPayloadId::ContractCall => {
                let call = TransactionContractCall::from_bytes(id.0)?;
                (call.0, Self::ContractCall(call.1))
            }
        };
        Ok(res)
    }

    pub fn is_token_transfer_payload(&self) -> bool {
        match self {
            Self::TokenTransfer(_) => true,
            _ => false,
        }
    }

    pub fn is_smart_contract_payload(&self) -> bool {
        match self {
            Self::SmartContract(_) => true,
            _ => false,
        }
    }
    pub fn is_contract_call_payload(&self) -> bool {
        match self {
            Self::ContractCall(_) => true,
            _ => false,
        }
    }

    pub fn contract_name(&self) -> Option<&[u8]> {
        match self {
            Self::SmartContract(ref contract) => contract.contract_name().ok(),
            Self::ContractCall(ref contract) => contract.contract_name().ok(),
            _ => None,
        }
    }

    pub fn function_name(&self) -> Option<&[u8]> {
        match self {
            Self::ContractCall(ref contract) => contract.function_name().ok(),
            _ => None,
        }
    }

    pub fn num_args(&self) -> Option<u32> {
        match self {
            Self::ContractCall(ref contract) => contract.num_args().ok(),
            _ => None,
        }
    }

    pub fn amount(&self) -> Option<u64> {
        match self {
            Self::TokenTransfer(ref token) => token.amount().ok(),
            _ => None,
        }
    }

    pub fn memo(&self) -> Option<&[u8]> {
        match self {
            Self::TokenTransfer(ref token) => Some(token.memo()),
            _ => None,
        }
    }

    pub fn recipient_address(&self) -> Option<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>> {
        match self {
            Self::TokenTransfer(ref token) => token.encoded_address().ok(),
            _ => None,
        }
    }
    pub fn contract_address(&self) -> Option<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>> {
        match self {
            Self::ContractCall(ref call) => call.contract_address().ok(),
            _ => None,
        }
    }

    pub fn num_items(&self) -> u8 {
        match self {
            Self::TokenTransfer(_) => 3,
            Self::SmartContract(_) => 1,
            Self::ContractCall(_) => 3,
        }
    }

    pub fn get_items(
        &self,
        display_idx: u8,
        out_key: &mut [u8],
        out_value: &mut [u8],
        page_idx: u8,
    ) -> Result<u8, ParserError> {
        let idx = display_idx % self.num_items();
        match self {
            Self::TokenTransfer(ref token) => {
                token.get_token_transfer_items(idx, out_key, out_value, page_idx)
            }
            Self::SmartContract(ref contract) => {
                contract.get_contract_items(idx, out_key, out_value, page_idx)
            }
            Self::ContractCall(ref call) => {
                check_canary!();
                call.get_contract_call_items(idx, out_key, out_value, page_idx)
            }
        }
    }
}

#[cfg(test)]
mod test {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn test_transaction_payload_tokens() {
        let bytes: Vec<u8> = vec![
            0, 5, 1, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 123, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];

        let parsed = TransactionPayload::from_bytes(&bytes).unwrap().1;
        assert_eq!(parsed.amount(), Some(123));
    }
}
