use crate::types::{Block, Bytes, SignedTransaction};

macro_rules! batch_msg_type {
    ($name: ident, $ty: ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name(pub Vec<$ty>);

        impl crate::traits::MessageCodec for $name {
            fn encode_msg(&mut self) -> crate::ProtocolResult<Bytes> {
                let bytes = rlp::encode_list(&self.0);
                Ok(bytes.freeze())
            }

            fn decode_msg(bytes: Bytes) -> crate::ProtocolResult<Self> {
                let inner: Vec<$ty> = rlp::Rlp::new(bytes.as_ref())
                    .as_list()
                    .map_err(|e| crate::codec::error::CodecError::Rlp(e.to_string()))?;
                Ok(Self(inner))
            }
        }

        impl $name {
            pub fn new(inner: Vec<$ty>) -> Self {
                Self(inner)
            }

            pub fn inner(self) -> Vec<$ty> {
                self.0
            }
        }
    };
}

batch_msg_type!(BatchSignedTxs, SignedTransaction);
batch_msg_type!(BatchBlocks, Block);

#[cfg(test)]
mod tests {
    use super::*;

    use rand::{random, rngs::OsRng};
    use rlp::Encodable;

    use common_crypto::{
        Crypto, PrivateKey, Secp256k1Recoverable, Secp256k1RecoverablePrivateKey, Signature,
    };

    use crate::codec::ProtocolCodec;
    use crate::types::{
        Eip1559Transaction, SignatureComponents, TransactionAction, UnsignedTransaction,
        UnverifiedTransaction,
    };

    fn mock_sign_tx() -> SignedTransaction {
        let mut utx = UnverifiedTransaction {
            unsigned:  UnsignedTransaction::Eip1559(Eip1559Transaction {
                nonce:                    Default::default(),
                max_priority_fee_per_gas: Default::default(),
                gas_price:                Default::default(),
                gas_limit:                Default::default(),
                action:                   TransactionAction::Create,
                value:                    Default::default(),
                data:                     Bytes::new(),
                access_list:              vec![],
            }),
            signature: Some(SignatureComponents {
                standard_v: 4,
                r:          Default::default(),
                s:          Default::default(),
            }),
            chain_id:  random::<u64>(),
            hash:      Default::default(),
        }
        .calc_hash();

        let priv_key = Secp256k1RecoverablePrivateKey::generate(&mut OsRng);
        let signature = Secp256k1Recoverable::sign_message(
            utx.signature_hash(true).as_bytes(),
            &priv_key.to_bytes(),
        )
        .unwrap()
        .to_bytes();
        utx.signature = Some(signature.into());

        utx.try_into().unwrap()
    }

    #[test]
    fn test_codec() {
        let stx = mock_sign_tx();
        let raw = stx.rlp_bytes();
        let decode = SignedTransaction::decode(raw).unwrap();
        assert_eq!(stx, decode);
    }
}
