use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use sapio::contract::object::BadTxIndex;
use std::collections::HashMap;

use bitcoin::util::bip32::*;
use sapio::clause::Clause;
use sapio::contract::emulator::CTVEmulator;
use sapio::contract::error::CompilationError;
use std::io::Read;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use bitcoin::consensus::encode::{Decodable, Encodable};
use bitcoin::secp256k1::{All, Secp256k1};
use bitcoin::util::psbt::PartiallySignedTransaction;
use sapio::template::CTVHash;
use std::thread;
thread_local! {
    pub static SECP: Secp256k1<All> = Secp256k1::new();
}

fn hash_to_child_vec(h: Sha256) -> Vec<ChildNumber> {
    let a: [u8; 32] = h.into_inner();
    let b: [[u8; 4]; 8] = unsafe { std::mem::transmute(a) };
    let mut c: Vec<ChildNumber> = b
        .iter()
        // Note: We mask off the top bit. This removes 8 bits of entropy from the hash,
        // but we add it back in later.
        .map(|x| (u32::from_be_bytes(*x) << 1) >> 1)
        .map(ChildNumber::from)
        .collect();
    // Add a unique 9th path for the MSB's
    c.push(
        b.iter()
            .enumerate()
            .map(|(i, x)| (u32::from_be_bytes(*x) >> 31) << i)
            .sum::<u32>()
            .into(),
    );
    c
}
#[derive(Clone)]
pub struct HDOracleEmulator {
    root: ExtendedPrivKey,
}

impl HDOracleEmulator {
    pub fn new(root: ExtendedPrivKey) -> Self {
        HDOracleEmulator { root }
    }
    pub async fn bind<A: ToSocketAddrs>(self, a: A) -> std::io::Result<()> {
        let listener = TcpListener::bind(a).await?;
        loop {
            let (mut socket, _) = listener.accept().await?;
            {
                let this = self.clone();
                tokio::spawn(async move {
                    loop {
                        this.handle(&mut socket).await;
                    }
                });
            }
        }
        Ok(())
    }
    fn derive(&self, h: Sha256, secp: &Secp256k1<All>) -> Result<ExtendedPrivKey, Error> {
        let c = hash_to_child_vec(h);
        self.root.derive_priv(secp, &c)
    }

    fn sign(
        &self,
        mut b: PartiallySignedTransaction,
        secp: &Secp256k1<All>,
    ) -> PartiallySignedTransaction {
        let tx = b.clone().extract_tx();
        let h = tx.get_ctv_hash(0);
        if let Ok(key) = self.derive(h, secp) {
            let pk = key.private_key.public_key(secp);
            let sighash = bitcoin::util::bip143::SighashComponents::new(&tx);

            if let Some(scriptcode) = &b.inputs[0].witness_script {
                if let Some(utxo) = &b.inputs[0].witness_utxo {
                    let sighash = sighash.sighash_all(&tx.input[0], &scriptcode, utxo.value);
                    let msg = bitcoin::secp256k1::Message::from_slice(&sighash[..]).unwrap();
                    let mut signature: Vec<u8> = secp
                        .sign(&msg, &key.private_key.key)
                        .serialize_compact()
                        .into();
                    signature.push(0x01);
                    b.inputs[0].partial_sigs.insert(pk, signature);
                    return b;
                }
            }
        }
        b
    }
    const MAX_MSG: usize = 1_000_000;
    async fn handle(&self, t: &mut TcpStream) -> Result<(), std::io::Error> {
        let len = t.read_u32().await? as usize;
        if len > Self::MAX_MSG {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid Length",
            ));
        }
        let mut m = vec![0; len];
        let read = t.read_exact(&mut m[..]).await?;
        if read != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid Length",
            ));
        }

        let psbt: PartiallySignedTransaction = Decodable::consensus_decode(&m[..]).unwrap();
        m.clear();
        let b = SECP.with(|secp| self.sign(psbt, secp));
        b.consensus_encode(&mut m);
        t.write_u32(m.len() as u32).await?;
        t.write_all(&m[..]).await?;
        Ok(())
    }
}
use std::sync::Arc;
struct HDOracleEmulatorConnection {
    runtime: Arc<tokio::runtime::Runtime>,
    connection: Mutex<Option<TcpStream>>,
    reconnect: SocketAddr,
    root: ExtendedPubKey,
    secp: bitcoin::secp256k1::Secp256k1<bitcoin::secp256k1::All>,
}

impl HDOracleEmulatorConnection {
    fn derive(&self, h: Sha256) -> Result<ExtendedPubKey, Error> {
        let c = hash_to_child_vec(h);
        self.root.derive_pub(&self.secp, &c)
    }
    async fn new<A: ToSocketAddrs>(
        address: A,
        root: ExtendedPubKey,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, std::io::Error> {
        Ok(HDOracleEmulatorConnection {
            connection: Mutex::new(None),
            reconnect: tokio::net::lookup_host(address).await?.next().ok_or(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "Bad Lookup"),
            )?,
            runtime,
            root,
            secp: Secp256k1::new(),
        })
    }
}
use tokio::sync::Mutex;
impl CTVEmulator for HDOracleEmulatorConnection {
    fn get_signer_for(
        &self,
        h: Sha256,
    ) -> Result<sapio::clause::Clause, sapio::contract::error::CompilationError> {
        Ok(Clause::Key(
            self.derive(h).map_err(CompilationError::custom)?.public_key,
        ))
    }
    fn sign(&self, mut b: PartiallySignedTransaction) -> PartiallySignedTransaction {
        let mut out = vec![];
        b.consensus_encode(&mut out);
        let res: Result<PartiallySignedTransaction, _> = self.runtime.block_on(async {
            let mut mconn = self.connection.lock().await;
            if let None = *mconn {
                *mconn = Some(TcpStream::connect(&self.reconnect).await?);
            }
            let conn: &mut TcpStream = &mut mconn.as_mut().unwrap();
            conn.write_u32(out.len() as u32).await?;
            conn.write_all(&out[..]).await?;
            let len = conn.read_u32().await? as usize;
            let mut inp = vec![0; len];
            if len != conn.read_exact(&mut inp[..]).await? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid Length",
                ));
            }
            Decodable::consensus_decode(&inp[..])
                .map_err(|_e| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid PSBT"))
        });
        match res {
            Ok(pb) => {
                b.merge(pb);
                b
            }
            Err(_) => b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitcoin::util::amount::Amount;
    use std::rc::Rc;
    use std::str::FromStr;

    use sapio::contract::*;
    use sapio::*;

    pub struct TestEmulation<T> {
        pub to_contract: T,
        pub amount: Amount,
        pub timeout: u32,
    }

    impl<T> TestEmulation<T>
    where
        T: Compilable,
    {
        then!(
            complete | s,
            ctx | {
                ctx.template()
                    .add_output(s.amount, &s.to_contract, None)?
                    .set_sequence(0, s.timeout)
                    .into()
            }
        );
    }

    impl<T: Compilable + 'static> Contract for TestEmulation<T> {
        declare! {then, Self::complete}
        declare! {non updatable}
    }

    #[test]
    fn test_connect() {
        let root =
            ExtendedPrivKey::new_master(bitcoin::network::constants::Network::Regtest, &[44u8; 32])
                .unwrap();
        let pk_root = ExtendedPubKey::from_private(&Secp256k1::new(), &root);
        {
            let RT = Arc::new(tokio::runtime::Runtime::new().unwrap());
            std::thread::spawn(move || {
                RT.block_on(async {
                    let oracle = HDOracleEmulator { root };
                    oracle.bind("127.0.0.1:8080").await;
                })
            });
        }

        let contract_1 = TestEmulation {
            to_contract: Compiled::from_address(
                bitcoin::Address::from_str("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh").unwrap(),
                None,
            ),
            amount: Amount::from_btc(1.0).unwrap(),
            timeout: 6,
        };
        let contract = TestEmulation {
            to_contract: contract_1,
            amount: Amount::from_btc(1.0).unwrap(),
            timeout: 4,
        };
        let RT2 = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let connecter = RT2.block_on(async {
            HDOracleEmulatorConnection::new("127.0.0.1:8080", pk_root, RT2.clone())
                .await
                .unwrap()
        });
        let rc_conn = (Rc::new(connecter));
        let compiled = contract
            .compile(&Context::new(
                Amount::from_btc(1.0).unwrap(),
                Some(rc_conn.clone()),
            ))
            .unwrap();
        let psbts = compiled.bind_psbt(
            bitcoin::OutPoint::default(),
            HashMap::new(),
            Rc::new(BadTxIndex::new()),
            rc_conn,
        );

        // TODO: Test PSBT result
    }
}