/******************************************************************************
 * Copyright © 2014-2018 The SuperNET Developers.                             *
 *                                                                            *
 * See the AUTHORS, DEVELOPER-AGREEMENT and LICENSE files at                  *
 * the top-level directory of this distribution for the individual copyright  *
 * holder information and the developer policies on copyright and licensing.  *
 *                                                                            *
 * Unless otherwise agreed in a custom licensing agreement, no part of the    *
 * SuperNET software, including this file may be copied, modified, propagated *
 * or distributed except according to the terms contained in the LICENSE file *
 *                                                                            *
 * Removal or modification of this copyright notice is prohibited.            *
 *                                                                            *
 ******************************************************************************/
//
//  utxo.rs
//  marketmaker
//
//  Copyright © 2017-2018 SuperNET. All rights reserved.
//

use base64::{encode_config as base64_encode, URL_SAFE};
use bitcoin_rpc::v1::types::{H256 as H256Json, Bytes as BytesJson, Transaction as RpcTransaction, VerboseBlockClient};
use bitcrypto::{dhash160};
use byteorder::{LittleEndian, WriteBytesExt};
use chain::{TransactionOutput, TransactionInput, OutPoint, Transaction as UtxoTransaction};
use chain::constants::{SEQUENCE_FINAL};
use common::{slurp_req, dstr};
use common::log::{LogState, StatusHandle};
use common::lp;
use futures::{Async, Future, Poll, Stream};
use gstuff::now_ms;
use hex::FromHex;
use hyper::{Body, Request, StatusCode};
use hyper::header::{AUTHORIZATION};
use keys::{KeyPair, Network, Private, Public, Address, Secret, Type};
use keys::bytes::Bytes;
use keys::generator::{Random, Generator};
use libc::{memcpy, c_void};
use primitives::hash::H256;
use script::{Opcode, Builder, Script, TransactionInputSigner, UnsignedTransactionInput, SignatureVersion};
use serialization::{serialize, deserialize};
use serde::de::DeserializeOwned;
use serde_json::{self as json, Value as Json};
use sha2::{Sha256, Digest};
use std::any::Any;
use std::cmp::Ordering;
use std::convert::AsMut;
use std::ffi::CStr;
use std::mem::transmute;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use super::{Transaction, ExchangeableCoin, BoxedTx, BoxedTxFut};
use tokio_timer::{Interval, Timer};

/// Clones slice into fixed size array
/// https://stackoverflow.com/a/37682288/8707622
fn clone_into_array<A: Default + AsMut<[T]>, T: Clone>(slice: &[T]) -> A {
    let mut a = Default::default();
    <A as AsMut<[T]>>::as_mut(&mut a).clone_from_slice(slice);
    a
}

#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct UnspentOutput {
    pub txid: H256Json,
    pub vout: u32,
    pub address: String,
    pub account: String,
    #[serde(rename = "scriptPubKey")]
    pub script_pub_key: BytesJson,
    pub amount: f64,
    pub confirmations: u64,
    pub spendable: bool
}

#[derive(Clone, Deserialize, Debug)]
pub struct ValidateAddressRes {
    #[serde(rename = "isvalid")]
    pub is_valid: bool,
    pub address: String,
    #[serde(rename = "scriptPubKey")]
    pub script_pub_key: BytesJson,
    #[serde(rename = "segid")]
    pub seg_id: Option<u32>,
    #[serde(rename = "ismine")]
    pub is_mine: bool,
    #[serde(rename = "iswatchonly")]
    pub is_watch_only: bool,
    #[serde(rename = "isscript")]
    pub is_script: bool,
    pub account: Option<String>,
}

/// Extended UTXO transaction, contains redeem script to spend p2sh output
/// Every transaction should contain separate redeem script for every p2sh output
/// However as MM creates only 1 p2sh output per every swap transaction
/// we can use single redeem script at least for now.
#[derive(Debug, Clone)]
pub struct ExtendedUtxoTx {
    pub transaction: UtxoTransaction,
    pub redeem_script: Bytes,
}

impl ExtendedUtxoTx {
    fn new() -> Self {
        ExtendedUtxoTx {
            transaction: UtxoTransaction {
                version: 1,
                lock_time: 0,
                inputs: vec![],
                outputs: vec![]
            },
            redeem_script: vec![].into(),
        }
    }

    pub fn transaction_bytes(&self) -> Bytes {
        serialize(&self.transaction)
    }
}

impl Transaction for ExtendedUtxoTx {
    fn to_raw_bytes(&self) -> Vec<u8> {
        let mut resulting_bytes = vec![];
        let tx_bytes = serialize(&self.transaction);
        let tx_len_bytes: [u8; 4] = unsafe { transmute(tx_bytes.len() as u32) };
        resulting_bytes.extend_from_slice(&tx_len_bytes);
        resulting_bytes.extend_from_slice(&tx_bytes);
        let redeem_len_bytes: [u8; 4] = unsafe { transmute(self.redeem_script.len() as u32) };
        resulting_bytes.extend_from_slice(&redeem_len_bytes);
        resulting_bytes.extend_from_slice(&self.redeem_script);
        resulting_bytes
    }

    fn box_clone(&self) -> Box<Transaction> {
        Box::new((*self).clone())
    }

    fn extract_secret(&self) -> Result<Vec<u8>, String> {
        let script: Script = self.transaction.outputs[0].script_pubkey.clone().into();
        for (i, instr) in script.iter().enumerate() {
            let instruction = instr.unwrap();
            if i == 1 {
                if instruction.opcode == Opcode::OP_PUSHBYTES_32 {
                    return Ok(instruction.data.unwrap().to_vec());
                }
            }
        }
        ERR!("Couldn't extract secret")
    }
}

/// Serializable RPC request
#[derive(Serialize, Debug)]
struct UtxoRpcRequest {
    jsonrpc: String,
    id: String,
    method: String,
    params: Vec<Json>,
}

impl UtxoRpcRequest {
    /// JSON RPC v1 request
    pub fn new_v1(method: String, params: Vec<Json>) -> UtxoRpcRequest {
        UtxoRpcRequest {
            jsonrpc: "1.0".to_owned(),
            id: "test".to_owned(),
            method,
            params,
        }
    }
}

type RpcRes<T> = Box<Future<Item=T, Error=String> + Send>;

/// Sends RPC request, returns a Future.
/// Errors in case of non-200 HTTP status code or if JSON rpc response has non-null error.
fn json_rpc_v1_request<T: DeserializeOwned + Send + 'static>(
    uri: &str,
    auth: &str,
    request: UtxoRpcRequest
) -> RpcRes<T> {
    let request_body = try_fus!(json::to_string(&request));
    let http_request = try_fus!(
        Request::builder()
                .method("POST")
                .header(
                    AUTHORIZATION,
                    auth.clone()
                )
                .uri(uri)
                .body(Body::from(request_body))
    );
    Box::new(slurp_req(http_request).then(move |result| -> Result<T, String> {
        let res = try_s!(result);
        let body = try_s!(std::str::from_utf8(&res.2));
        if res.0 != StatusCode::OK {
            return ERR!("Rpc request {:?} failed with HTTP status code {}, response body: {}",
                        request, res.0, body);
        }
        let json_body: Json = try_s!(json::from_str(body));
        if !json_body["error"].is_null() {
            return ERR!("Rpc request {:?} failed with error, response body: {}",
                        request, json_body);
        }
        Ok(try_s!(json::from_value(json_body["result"].clone())))
    }))
}

/// Macro generating functions for RPC v1 requests.
/// Args must implement/derive Serialize trait.
/// Generates params vector from input args, builds the request and sends it.
macro_rules! rpc_func {
    (pub fn $method:ident(&$selff:ident $(, $arg_name:ident: $arg_ty:ty)*) -> $return_ty:ty) => {
        pub fn $method(&$selff $(, $arg_name: $arg_ty)*) -> $return_ty {
            let mut params = vec![];
            $(
                params.push(try_fus!(json::value::to_value($arg_name)));
            )*
            let request = UtxoRpcRequest::new_v1(stringify!($method).replace("_", ""), params);
            json_rpc_v1_request(&$selff.uri, &$selff.auth, request)
        }
    }
}

/// RPC client for UTXO based coins
/// https://bitcoin.org/en/developer-reference#rpc-quick-reference - Bitcoin RPC API reference
/// Other coins have additional methods or miss some of these
/// This description will be updated with more info
#[derive(Clone, Debug)]
struct UtxoRpcClient {
    /// The uri to send requests to
    uri: String,
    /// Value of Authorization header, e.g. "Basic base64(user:password)"
    auth: String,
}

impl UtxoRpcClient {
    /// https://bitcoin.org/en/developer-reference#listunspent
    rpc_func!(pub fn list_unspent(&self, min_conf: u64, max_conf: u64, addresses: Vec<String>)
        -> RpcRes<Vec<UnspentOutput>>);

    pub fn list_unspent_ordered(
        &self,
        min_conf: u64,
        max_conf: u64,
        addresses: Vec<String>
    ) -> RpcRes<Vec<UnspentOutput>> {
        Box::new(self.list_unspent(min_conf, max_conf, addresses).and_then(move |mut unspents| {
            unspents.sort_unstable_by(|a, b| {
                if a.amount < b.amount {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            });
            futures::future::ok(unspents)
        }))
    }

    /// https://bitcoin.org/en/developer-reference#importaddress
    rpc_func!(pub fn import_address(&self, address: String, label: String, rescan: bool)
        -> RpcRes<()>);

    /// https://bitcoin.org/en/developer-reference#getblockcount
    rpc_func!(pub fn get_block_count(&self) -> RpcRes<u64>);

    /// https://bitcoin.org/en/developer-reference#sendrawtransaction
    rpc_func!(pub fn send_raw_transaction(&self, tx: BytesJson) -> RpcRes<H256Json>);

    /// https://bitcoin.org/en/developer-reference#validateaddress
    rpc_func!(pub fn validate_address(&self, address: String) -> RpcRes<ValidateAddressRes>);

    /// https://bitcoin.org/en/developer-reference#getrawtransaction
    /// It expects that verbose param is always 1 to get deserialized transaction
    rpc_func!(pub fn get_raw_transaction(&self, txid: H256Json, verbose: u32) -> RpcRes<RpcTransaction>);

    /// https://bitcoin.org/en/developer-reference#getblock
    /// It expects that verbose param is always true to get verbose block
    rpc_func!(pub fn get_block(&self, height: String, verbose: bool) -> RpcRes<VerboseBlockClient>);
}

#[derive(Debug)]
pub struct UtxoCoin {
    /// https://en.bitcoin.it/wiki/List_of_address_prefixes
    /// https://github.com/jl777/coins/blob/master/coins
    pub_type: u8,
    p2sh_type: u8,
    wif_type: u8,
    wif_t_addr: u8,
    t_addr: u8,
    /// True if coins uses Proof of Stake consensus algo
    /// Proof of Work is expected by default
    /// https://en.bitcoin.it/wiki/Proof_of_Stake
    /// https://en.bitcoin.it/wiki/Proof_of_work
    is_pos: bool,
    /// Special field for Zcash and it's forks
    /// Defines if Overwinter network upgrade was activated
    /// https://z.cash/upgrade/overwinter/
    overwintered: bool,
    /// The tx version used to detect the transaction ser/de/signing algo
    /// For now it's mostly used for Zcash and forks because they changed the algo in
    /// Overwinter and then Sapling upgrades
    /// https://github.com/zcash/zips/blob/master/zip-0243.rst
    tx_version: u32,
    /// If true - use Segwit protocol
    /// https://en.bitcoin.it/wiki/Segregated_Witness
    segwit: bool,
    /// Default decimals amount is 8 (BTC and almost all other UTXO coins)
    /// But there are forks which have different decimals:
    /// Peercoin has 6
    /// Emercoin has 6
    /// Bitcoin Diamond has 7
    decimals: u8,
    /// Is coin protected by Komodo dPoW?
    /// https://komodoplatform.com/security-delayed-proof-of-work-dpow/
    notarized: bool,
    /// RPC port
    rpc_port: u16,
    /// RPC username
    rpc_user: String,
    /// RPC password
    rpc_password: String,
    /// RPC client
    rpc_client: UtxoRpcClient,
    /// ECDSA key pair
    key_pair: KeyPair,
    /// Lock the mutex when we deal with address utxos
    utxo_mutex: Mutex<()>,
    my_address: Address
}

/// Only ETH and ERC20 tokens are supported currently
/// It's planned to support another ERC token standards
enum EthTokenType {
    /// The Ethereum itself or it's forks: ETC and others
    Base,
    /// ERC20 token: https://github.com/ethereum/EIPs/blob/master/EIPS/eip-20.md
    /// The string param defines to what base coin the token belongs (ETH, ETC or another fork)
    Erc20(String)
}

struct EthCoin {
    /// Default ETH decimals is 18 but tokens can have any number (even zero or > 18)
    decimals: u8,
    token_type: EthTokenType,
    /// The address of Smart contract representing Alice side. Raw bytes form
    alice_contract_address: Vec<u8>,
    /// The address of Smart contract representing Bob side. Raw bytes form
    bob_contract_address: Vec<u8>,
}

/// Generates unsigned transaction (TransactionInputSigner) from specified utxos and outputs.
/// This function expects that utxos are sorted by amounts in ascending order
/// Consider sorting before calling this function
fn generate_transaction(
    utxos: Vec<UnspentOutput>,
    mut outputs: Vec<TransactionOutput>,
    lock_time: u32,
    version: i32,
    tx_fee: u64,
    decimals: u8,
    change_script_pubkey: Bytes,
) -> Result<TransactionInputSigner, String> {
    if utxos.is_empty() {
        return ERR!("Couldn't generate tx from empty utxos set");
    }

    if outputs.is_empty() {
        return ERR!("Couldn't generate tx from empty outputs set");
    }

    let mut target_value = 0;
    for output in outputs.iter() {
        target_value += output.value;
    }

    if target_value == 0 {
        return ERR!("Total target value calculated from outputs {:?} is zero", outputs);
    }
    target_value += tx_fee;

    let mut value_to_spend = 0;
    let mut inputs = vec![];
    for utxo in utxos.iter() {
        value_to_spend += f64_to_sat(utxo.amount, decimals);
        inputs.push(UnsignedTransactionInput {
            previous_output: OutPoint {
                hash: utxo.txid.reversed().into(),
                index: utxo.vout,
            },
            sequence: SEQUENCE_FINAL
        });
        if value_to_spend >= target_value { break; }
    }

    if value_to_spend < target_value {
        return ERR!("Couldn't collect enough value from utxos {:?} to create tx with outputs {:?}", utxos, outputs);
    }

    if value_to_spend > target_value {
        outputs.push({
            TransactionOutput {
                value: value_to_spend - target_value,
                script_pubkey: change_script_pubkey
            }
        });
    }

    Ok(TransactionInputSigner {
        inputs,
        outputs,
        lock_time,
        version
    })
}

fn f64_to_sat(amount: f64, decimals: u8) -> u64 {
    (amount * 10u64.pow(decimals as u32) as f64) as u64
}

fn payment_script(
    time_lock: u32,
    secret_hash: &[u8],
    pub_0: &Public,
    pub_1: &Public
) -> Result<Script, String> {
    let builder = Builder::default();
    let mut wtr = vec![];
    try_s!(wtr.write_u32::<LittleEndian>(time_lock));
    Ok(builder
        .push_opcode(Opcode::OP_IF)
        .push_bytes(&wtr)
        .push_opcode(Opcode::OP_CHECKLOCKTIMEVERIFY)
        .push_opcode(Opcode::OP_DROP)
        .push_bytes(pub_0)
        .push_opcode(Opcode::OP_CHECKSIG)
        .push_opcode(Opcode::OP_ELSE)
        .push_opcode(Opcode::OP_SIZE)
        .push_bytes(&[32])
        .push_opcode(Opcode::OP_EQUALVERIFY)
        .push_opcode(Opcode::OP_HASH160)
        .push_bytes(secret_hash)
        .push_opcode(Opcode::OP_EQUALVERIFY)
        .push_bytes(pub_1)
        .push_opcode(Opcode::OP_CHECKSIG)
        .push_opcode(Opcode::OP_ENDIF)
        .into_script())
}

fn script_sig(message: &H256, key_pair: &KeyPair) -> Result<Bytes, String> {
    let signature = try_s!(key_pair.private().sign(message));

    let mut sig_script = Bytes::default();
    sig_script.append(&mut Bytes::from((*signature).to_vec()));
    // Using SIGHASH_ALL only for now
    sig_script.append(&mut Bytes::from(vec![1]));

    Ok(sig_script)
}

fn script_sig_with_pub(message: &H256, key_pair: &KeyPair) -> Result<Bytes, String> {
    let sig_script = try_s!(script_sig(message, key_pair));

    let builder = Builder::default();

    Ok(builder
        .push_data(&sig_script)
        .push_data(&key_pair.public().to_vec())
        .into_bytes())
}

/// Creates signed input spending p2pkh output
fn p2pkh_spend(
    signer: &TransactionInputSigner,
    input_index: usize,
    key_pair: &KeyPair,
    prev_script: &Script
) -> Result<TransactionInput, String> {
    let script = Builder::build_p2pkh(&key_pair.public().address_hash());
    if script != *prev_script {
        return ERR!("p2pkh script {} built from input key pair doesn't match expected prev script {}", script, prev_script);
    }
    let sighash = signer.signature_hash(input_index, 0, &script, SignatureVersion::Base, 1);

    let script_sig = try_s!(script_sig_with_pub(&sighash, key_pair));

    Ok(TransactionInput {
        script_sig,
        sequence: signer.inputs[input_index].sequence,
        script_witness: vec![],
        previous_output: signer.inputs[input_index].previous_output.clone()
    })
}

/// Creates signed input spending p2sh output
fn p2sh_spend(
    signer: &TransactionInputSigner,
    input_index: usize,
    key_pair: &KeyPair,
    script_data: Script,
    redeem_script: Script,
) -> Result<TransactionInput, String> {
    let sighash = signer.signature_hash(input_index, 0, &redeem_script, SignatureVersion::Base, 1);

    let sig = try_s!(script_sig(&sighash, &key_pair));

    let mut resulting_script = Builder::default().push_data(&sig).into_bytes();
    if !script_data.is_empty() {
        resulting_script.extend_from_slice(&script_data);
    }

    let redeem_part = Builder::default().push_data(&redeem_script).into_bytes();
    resulting_script.extend_from_slice(&redeem_part);

    Ok(TransactionInput {
        script_sig: resulting_script.into(),
        sequence: signer.inputs[input_index].sequence,
        script_witness: vec![],
        previous_output: signer.inputs[input_index].previous_output.clone()
    })
}

fn p2sh_spending_tx(
    prev_transaction: ExtendedUtxoTx,
    outputs: Vec<TransactionOutput>,
    script_data: Script,
    key_pair: &KeyPair,
    lock_time: u32,
    sequence: u32,
) -> Result<UtxoTransaction, String> {
    let unsigned = TransactionInputSigner {
        lock_time,
        version: 1,
        inputs: vec![UnsignedTransactionInput {
            sequence,
            previous_output: OutPoint {
                hash: prev_transaction.transaction.hash(),
                index: 0,
            }
        }],
        outputs: outputs.clone(),
    };
    let signed_input = try_s!(
        p2sh_spend(&unsigned, 0, key_pair, script_data, prev_transaction.redeem_script.into())
    );
    Ok(UtxoTransaction {
        version: unsigned.version,
        lock_time: unsigned.lock_time,
        inputs: vec![signed_input],
        outputs,
    })
}

fn address_from_raw_pubkey(pub_key: &[u8]) -> Result<Address, String> {
    Ok(Address {
        kind: Type::P2PKH,
        hash: try_s!(Public::from_slice(pub_key)).address_hash(),
        network: Network::Komodo
    })
}

fn sign_tx(
    unsigned: TransactionInputSigner,
    key_pair: &KeyPair,
    prev_script: Script
) -> Result<UtxoTransaction, String> {
    let mut signed_inputs = vec![];
    for (i, _) in unsigned.inputs.iter().enumerate() {
        signed_inputs.push(
            try_s!(p2pkh_spend(&unsigned, i, key_pair, &prev_script))
        );
    }
    Ok(UtxoTransaction {
        inputs: signed_inputs,
        outputs: unsigned.outputs.clone(),
        version: unsigned.version,
        lock_time: unsigned.lock_time
    })
}

#[derive(Debug)]
pub struct UtxoCoinArc(Arc<UtxoCoin>);
impl Deref for UtxoCoinArc {type Target = UtxoCoin; fn deref (&self) -> &UtxoCoin {&*self.0}}
impl Clone for UtxoCoinArc {fn clone (&self) -> UtxoCoinArc {UtxoCoinArc (self.0.clone())}}

impl UtxoCoinArc {
    fn send_outputs_from_my_address(&self, outputs: Vec<TransactionOutput>, redeem_script: Bytes) -> BoxedTxFut {
        let change_script_pubkey = Builder::build_p2pkh(&self.key_pair.public().address_hash()).to_bytes();
        let arc = self.clone();
        let unspent_fut = self.rpc_client.list_unspent_ordered(0, 999999, vec![self.my_address.to_string()]);
        Box::new(unspent_fut.then(move |unspents| -> BoxedTxFut {
            let unspents = try_fus!(unspents);
            let unsigned = try_fus!(generate_transaction(
                unspents,
                outputs,
                0,
                1,
                1000,
                arc.decimals,
                change_script_pubkey.clone()
            ));
            let signed = try_fus!(sign_tx(unsigned, &arc.key_pair, change_script_pubkey.into()));
            let tx = Box::new(ExtendedUtxoTx {
                transaction: signed,
                redeem_script
            });
            arc.send_raw_tx(tx)
        }))
    }
}

fn compressed_key_pair_from_bytes(raw: &[u8]) -> Result<KeyPair, String> {
    if raw.len() != 32 {
        return ERR!("Invalid raw priv key len {}", raw.len());
    }

    let private = Private {
        network: Network::Komodo,
        compressed: true,
        secret: Secret::from(raw)
    };
    Ok(try_s!(KeyPair::from_private(private)))
}

impl ExchangeableCoin for UtxoCoinArc {
    fn send_buyer_fee(&self, fee_pub_key: &[u8], amount: f64) -> BoxedTxFut {
        let address = try_fus!(address_from_raw_pubkey(fee_pub_key));
        let output = TransactionOutput {
            value: f64_to_sat(amount, self.decimals),
            script_pubkey: Builder::build_p2pkh(&address.hash).to_bytes()
        };
        self.send_outputs_from_my_address(vec![output], vec![].into())
    }

    fn send_buyer_payment(
        &self,
        time_lock: u32,
        pub_a0: &[u8],
        pub_b0: &[u8],
        priv_bn_hash: &[u8],
        amount: f64,
    ) -> BoxedTxFut {
        let redeem_script = try_fus!(payment_script(
            time_lock,
            priv_bn_hash,
            &try_fus!(Public::from_slice(pub_a0)),
            &try_fus!(Public::from_slice(pub_b0)),
        ));
        let output = TransactionOutput {
            value: f64_to_sat(amount, self.decimals),
            script_pubkey: Builder::build_p2sh(&dhash160(&redeem_script)).into(),
        };
        self.send_outputs_from_my_address(vec![output], redeem_script.into())
    }

    fn send_seller_payment(
        &self,
        time_lock: u32,
        pub_a0: &[u8],
        pub_b0: &[u8],
        priv_bn_hash: &[u8],
        amount: f64,
    ) -> BoxedTxFut {
        let redeem_script = try_fus!(payment_script(
            time_lock,
            priv_bn_hash,
            &try_fus!(Public::from_slice(pub_b0)),
            &try_fus!(Public::from_slice(pub_a0)),
        ));
        let output = TransactionOutput {
            value: f64_to_sat(amount, self.decimals),
            script_pubkey: Builder::build_p2sh(&dhash160(&redeem_script)).into(),
        };
        self.send_outputs_from_my_address(vec![output], redeem_script.into())
    }

    fn send_seller_spends_buyer_payment(
        &self,
        buyer_payment_tx: BoxedTx,
        b_priv_0: &[u8],
        b_priv_n: &[u8],
        amount: f64
    ) -> BoxedTxFut {
        let prev_tx: Box<ExtendedUtxoTx> = downcast_fus!(buyer_payment_tx);
        let key_pair = try_fus!(compressed_key_pair_from_bytes(b_priv_0));
        let output = TransactionOutput {
            value: prev_tx.transaction.outputs[0].value - 1000,
            script_pubkey: Builder::build_p2pkh(&self.key_pair.public().address_hash()).to_bytes()
        };
        let script_data = Builder::default()
            .push_data(b_priv_n)
            .push_opcode(Opcode::OP_0)
            .into_script();
        let transaction = try_fus!(p2sh_spending_tx(
            *prev_tx,
            vec![output],
            script_data,
            &key_pair,
            (now_ms() / 1000) as u32,
            SEQUENCE_FINAL
        ));
        self.send_raw_tx(Box::new(ExtendedUtxoTx {
            transaction,
            redeem_script: vec![].into()
        }))
    }

    fn send_buyer_spends_seller_payment(
        &self,
        seller_payment_tx: BoxedTx,
        a_priv_0: &[u8],
        b_priv_n: &[u8],
        amount: f64
    ) -> BoxedTxFut {
        let prev_tx: Box<ExtendedUtxoTx> = downcast_fus!(seller_payment_tx);
        let key_pair = try_fus!(compressed_key_pair_from_bytes(a_priv_0));
        let output = TransactionOutput {
            value: prev_tx.transaction.outputs[0].value - 1000,
            script_pubkey: Builder::build_p2pkh(&self.key_pair.public().address_hash()).to_bytes()
        };
        let script_data = Builder::default()
            .push_data(b_priv_n)
            .push_opcode(Opcode::OP_0)
            .into_script();
        let transaction = try_fus!(p2sh_spending_tx(
            *prev_tx,
            vec![output],
            script_data,
            &key_pair,
            (now_ms() / 1000) as u32,
            SEQUENCE_FINAL
        ));
        self.send_raw_tx(Box::new(ExtendedUtxoTx {
            transaction,
            redeem_script: vec![].into()
        }))
    }

    fn send_buyer_refunds_payment(
        &self,
        buyer_payment_tx: BoxedTx,
        a_priv_0: &[u8],
        amount: f64
    ) -> BoxedTxFut {
        let prev_tx: Box<ExtendedUtxoTx> = downcast_fus!(buyer_payment_tx);
        let key_pair = try_fus!(compressed_key_pair_from_bytes(a_priv_0));
        let output = TransactionOutput {
            value: prev_tx.transaction.outputs[0].value - 1000,
            script_pubkey: Builder::build_p2pkh(&self.key_pair.public().address_hash()).to_bytes()
        };
        let script_data = Builder::default()
            .push_opcode(Opcode::OP_1)
            .into_script();
        let transaction = try_fus!(p2sh_spending_tx(
            *prev_tx,
            vec![output],
            script_data,
            &key_pair,
            (now_ms() / 1000) as u32,
            SEQUENCE_FINAL - 1
        ));
        self.send_raw_tx(Box::new(ExtendedUtxoTx {
            transaction,
            redeem_script: vec![].into()
        }))
    }

    fn send_seller_refunds_payment(
        &self,
        seller_payment_tx: BoxedTx,
        b_priv_0: &[u8],
        amount: f64
    ) -> BoxedTxFut {
        let prev_tx: Box<ExtendedUtxoTx> = downcast_fus!(seller_payment_tx);
        let key_pair = try_fus!(compressed_key_pair_from_bytes(b_priv_0));
        let output = TransactionOutput {
            value: prev_tx.transaction.outputs[0].value - 1000,
            script_pubkey: Builder::build_p2pkh(&self.key_pair.public().address_hash()).to_bytes()
        };
        let script_data = Builder::default()
            .push_opcode(Opcode::OP_1)
            .into_script();
        let transaction = try_fus!(p2sh_spending_tx(
            *prev_tx,
            vec![output],
            script_data,
            &key_pair,
            (now_ms() / 1000) as u32,
            SEQUENCE_FINAL - 1
        ));
        self.send_raw_tx(Box::new(ExtendedUtxoTx {
            transaction,
            redeem_script: vec![].into()
        }))
    }

    fn get_balance(&self) -> f64 { unimplemented!() }

    fn send_raw_tx(&self, tx: BoxedTx) -> BoxedTxFut {
        let tx: Box<ExtendedUtxoTx> = downcast_fus!(tx);
        println!("Raw tx {:?}", tx.transaction);
        println!("Hash {}", tx.transaction.hash().reversed());
        let send_fut = self.rpc_client.send_raw_transaction(BytesJson::from(serialize(&tx.transaction)));
        Box::new(
            send_fut.then(move |res| -> Result<BoxedTx, String> {
                let res = try_s!(res);
                Ok(tx)
            })
        )
    }

    fn wait_for_confirmations(
        &self,
        tx: BoxedTx,
        confirmations: i32,
    ) -> Box<dyn Future<Item=(), Error=String>> {
        let tx: Box<ExtendedUtxoTx> = downcast_fus!(tx);
        Box::new(WaitForUtxoTxConfirmations::new(
            self.clone(),
            tx.transaction.hash().reversed().into(),
            10,
            now_ms() / 1000 + 1000,
            confirmations,
            10
        ))
    }

    fn wait_for_tx_spend(&self, transaction: BoxedTx, wait_until: u64) -> BoxedTxFut {
        let tx: Box<ExtendedUtxoTx> = downcast_fus!(transaction);
        Box::new(WaitForTxSpend::new(
            self.clone(),
            tx.transaction.hash().reversed().into(),
            0,
            10,
            now_ms() / 1000 + 1000,
            10,
            131400,
        ))
    }

    fn tx_from_raw_bytes(&self, bytes: &[u8]) -> Result<BoxedTx, String> {
        // should be at least 8 bytes length in case tx and redeem length is zero
        if bytes.len() < 8 {
            return ERR!("Input bytes slice len is too small");
        }
        let len_array = clone_into_array::<[u8; 4], u8>(&bytes[0..4]);
        let tx_len: u32 = unsafe { transmute(len_array) };
        let mut read: usize = 4;
        let transaction: UtxoTransaction = try_s!(deserialize(&bytes[read..read + tx_len as usize]).map_err(|err| format!("{:?}", err)));
        read += tx_len as usize;
        let redeem_len: u32 = unsafe { transmute(clone_into_array::<[u8; 4], u8>(&bytes[read..read + 4])) };
        read += 4 as usize;
        let redeem_script = Bytes::from(&bytes[read..read + redeem_len as usize]);
        Ok(Box::new(ExtendedUtxoTx {
            transaction,
            redeem_script,
        }))
    }
}

fn random_compressed_key_pair() -> Result<KeyPair, String> {
    let random_key = try_s!(Random::new(Network::Komodo).generate());

    Ok(try_s!(KeyPair::from_private(Private {
        network: Network::Komodo,
        secret: random_key.private().secret.clone(),
        compressed: true,
    })))
}

fn key_pair_from_seed(seed: &[u8]) -> KeyPair {
    let mut hasher = Sha256::new();
    hasher.input(seed);
    let mut hash = hasher.result();
    hash[0] &= 248;
    hash[31] &= 127;
    hash[31] |= 64;
    let private = Private {
        network: Network::Komodo,
        secret: H256::from(hash.as_slice()),
        compressed: true,
    };

    KeyPair::from_private(private).unwrap()
}

pub fn coin_from_iguana_info(info: *mut lp::iguana_info) -> Result<Box<ExchangeableCoin>, String> {
    let info = unsafe { *info };
    let auth_str = unsafe { try_s!(CStr::from_ptr(info.userpass.as_ptr()).to_str()) };
    let uri = unsafe { try_s!(CStr::from_ptr(info.serverport.as_ptr()).to_str()) };
    let private = Private {
        network: Network::Komodo,
        secret: H256::from(unsafe { lp::G.LP_privkey.bytes }),
        compressed: true,
    };

    let key_pair = try_s!(KeyPair::from_private(private));
    let my_address = Address {
        network: Network::Komodo,
        hash: key_pair.public().address_hash(),
        kind: Type::P2PKH
    };
    let coin = UtxoCoin {
        decimals: 8,
        rpc_client: UtxoRpcClient {
            uri: format!("http://{}", uri),
            auth: format!("Basic {}", base64_encode(auth_str, URL_SAFE)),
        },
        key_pair,
        is_pos: false,
        notarized: false,
        overwintered: false,
        p2sh_type: 0,
        pub_type: 0,
        rpc_password: "".to_owned(),
        rpc_port: 0,
        rpc_user: "".to_owned(),
        segwit: false,
        t_addr: 0,
        wif_t_addr: 0,
        wif_type: 0,
        tx_version: 1,
        utxo_mutex: Mutex::new(()),
        my_address: my_address.clone(),
    };
    Ok(Box::new(UtxoCoinArc(Arc::new(coin))))
}

/// Temporary in memory LogState instance, consider replacing with LogState instance from MmCtx
lazy_static!(
    pub static ref MEMORY_LOG: LogState = LogState::in_memory();
);

enum WaitForConfirmationState {
    WaitingForInterval,
    CheckingConfirmations(RpcRes<RpcTransaction>),
}

struct WaitForUtxoTxConfirmations<'a> {
    coin: UtxoCoinArc,
    txid: H256Json,
    interval: Interval,
    wait_until: u64,
    status: StatusHandle<'a>,
    confirmations: i32,
    retries: u8,
    max_retries: u8,
    state: WaitForConfirmationState
}

impl<'a> WaitForUtxoTxConfirmations<'a> {
    pub fn new(
        coin: UtxoCoinArc,
        txid: H256Json,
        poll_interval: u64,
        wait_until: u64,
        confirmations: i32,
        max_retries: u8,
    ) -> Self {
        WaitForUtxoTxConfirmations {
            coin,
            status: MEMORY_LOG.status(&[&"transaction", &(format!("{:?}", txid), "waiting")], "Waiting for confirmations..."),
            txid,
            interval: Timer::default().interval(Duration::from_secs(poll_interval)),
            wait_until,
            confirmations,
            retries: 0,
            max_retries,
            state: WaitForConfirmationState::WaitingForInterval,
        }
    }
}

impl<'a> Future for WaitForUtxoTxConfirmations<'a> {
    type Item = ();
    type Error = String;

    fn poll(&mut self) -> Poll<(), String> {
        loop {
            let next_state = match self.state {
                WaitForConfirmationState::WaitingForInterval => {
                    if now_ms() / 1000 > self.wait_until {
                        return ERR!("Waited too long until {}, aborted", self.wait_until);
                    }
                    let _ready = try_ready!(
                        self.interval
                            .poll()
                            .map_err(|e| {
                                ERRL!("{}", e)
                            })
                    );
                    WaitForConfirmationState::CheckingConfirmations(
                        self.coin.rpc_client.get_raw_transaction(self.txid.clone(), 1)
                    )
                },
                WaitForConfirmationState::CheckingConfirmations(ref mut future) => {
                    let tx = future.poll();
                    match tx {
                        Ok(Async::Ready(transaction)) => {
                            if transaction.confirmations as i32 >= self.confirmations {
                                self.status.append("Reached required confirmations");
                                return Ok(Async::Ready(()))
                            } else {
                                self.status.append(
                                    &format!(
                                        "Confirmed {} times, target {}..",
                                        transaction.confirmations,
                                        self.confirmations
                                    )
                                );
                            }
                        },
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(e) => {
                            self.status.append(&format!("Attempt {}, got error {}..", self.retries, e));
                            self.retries += 1;
                            if self.retries >= self.max_retries {
                                self.status.append("Reached max attempts count, aborting..");
                                return ERR!("Error waiting for tx confirmation {:?}", self.txid)
                            }
                        }
                    }
                    WaitForConfirmationState::WaitingForInterval
                },
            };
            self.state = next_state;
        }
    }
}

enum WaitForTxSpendState {
    WaitForInterval,
    GetBlockCount(RpcRes<u64>),
    GetBlock(RpcRes<VerboseBlockClient>),
}

struct WaitForTxSpend<'a> {
    coin: UtxoCoinArc,
    txid: H256Json,
    vout: u32,
    interval: Interval,
    wait_until: u64,
    status: StatusHandle<'a>,
    retries: u8,
    max_retries: u8,
    state: WaitForTxSpendState,
    current_height: u64,
}

impl<'a> WaitForTxSpend<'a> {
    pub fn new(
        coin: UtxoCoinArc,
        txid: H256Json,
        vout: u32,
        poll_interval: u64,
        wait_until: u64,
        max_retries: u8,
        current_height: u64,
    ) -> Self {
        let fut = coin.rpc_client.get_block(current_height.to_string(), true);
        WaitForTxSpend {
            coin,
            status: MEMORY_LOG.status(&[&"transaction", &(format!("{:?}:{}", txid, vout), "waiting")], "Waiting for tx spend..."),
            txid,
            vout,
            interval: Timer::default().interval(Duration::from_secs(poll_interval)),
            wait_until,
            retries: 0,
            max_retries,
            state: WaitForTxSpendState::GetBlock(fut),
            current_height
        }
    }
}

impl<'a> Future for WaitForTxSpend<'a> {
    type Item = BoxedTx;
    type Error = String;

    fn poll(&mut self) -> Poll<BoxedTx, String> {
        loop {
            let next_state = match self.state {
                WaitForTxSpendState::WaitForInterval => {
                    if now_ms() / 1000 > self.wait_until {
                        return ERR!("Waited too long until {}, aborted", self.wait_until);
                    }
                    let _ready = try_ready!(
                        self.interval
                            .poll()
                            .map_err(|e| {
                                ERRL!("{}", e)
                            })
                    );
                    WaitForTxSpendState::GetBlockCount(
                        self.coin.rpc_client.get_block_count()
                    )
                },
                WaitForTxSpendState::GetBlockCount(ref mut future) => {
                    let height = try_ready!(future.poll());
                    if self.current_height < height {
                        self.current_height += 1;
                        let get_block_fut = self.coin.rpc_client.get_block(self.current_height.to_string(), true);
                        WaitForTxSpendState::GetBlock(get_block_fut)
                    } else {
                        WaitForTxSpendState::WaitForInterval
                    }
                },
                WaitForTxSpendState::GetBlock(ref mut future) => {
                    let block = try_ready!(future.poll());
                    for tx in block.tx.iter() {
                        // TODO replace it with poll() per every Future later
                        let transaction = match self.coin.rpc_client.get_raw_transaction(tx.clone(), 1).wait() {
                            Ok(tx) => tx,
                            Err(_e) => continue
                        };
                        for input in transaction.vin.iter() {
                            if input.txid == self.txid && input.vout == self.vout {
                                let result = Box::new(ExtendedUtxoTx {
                                    transaction: try_s!(deserialize(transaction.hex.as_slice()).map_err(|e| format!("{:?}", e))),
                                    redeem_script: vec![].into()
                                });
                                return Ok(Async::Ready(result))
                            }
                        }
                    }
                    WaitForTxSpendState::WaitForInterval
                },
            };
            self.state = next_state;
        }
    }
}

pub fn coin_from_json() -> Result<Box<ExchangeableCoin>, String> {
    /*    if json["etomic"].is_string() {
        Ok(Box::new(EthCoin {
            decimals: 18,
            alice_contract_address: vec![],
            bob_contract_address: vec![],
            token_type: EthTokenType::Base
        }))
    } else {*/
    let key_pair = key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid".as_bytes());
    let my_address = Address {
        network: Network::Komodo,
        hash: key_pair.public().address_hash(),
        kind: Type::P2PKH
    };
    let coin = UtxoCoin {
        decimals: 8,
        rpc_client: UtxoRpcClient {
            uri: "http://127.0.0.1:10271".to_owned(),
            auth: format!("Basic {}", base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE)),
        },
        key_pair,
        is_pos: false,
        notarized: false,
        overwintered: false,
        p2sh_type: 0,
        pub_type: 0,
        rpc_password: "".to_owned(),
        rpc_port: 0,
        rpc_user: "".to_owned(),
        segwit: false,
        t_addr: 0,
        wif_t_addr: 0,
        wif_type: 0,
        tx_version: 1,
        utxo_mutex: Mutex::new(()),
        my_address,
    };

    Ok(Box::new(UtxoCoinArc(Arc::new(coin))))
    //}
}

#[test]
fn test_send_buyer_fee() {
    let coin = coin_from_json().unwrap();
    let tx = coin.send_buyer_fee(
        &hex::decode("02031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3").unwrap(),
        0.1
    ).wait().unwrap();
    // println!("{:?}", tx);
}

#[test]
fn test_send_and_refund_buyer_payment() {
    let coin = coin_from_json().unwrap();
    let priv_bn = unwrap!(random_compressed_key_pair());
    let priv_b0 = unwrap!(random_compressed_key_pair());
    let priv_a0 = unwrap!(random_compressed_key_pair());
    let tx = coin.send_buyer_payment(
        (now_ms() / 1000) as u32,
        &priv_a0.public().to_vec(),
        &priv_b0.public().to_vec(),
        &dhash160(&*priv_bn.private().secret).to_vec(),
        0.001
    ).wait().unwrap();

    let refund_tx = coin.send_buyer_refunds_payment(
        tx,
        &priv_a0.private().secret.to_vec(),
        0.0999
    ).wait().unwrap();
}

#[test]
fn test_send_and_spend_buyer_payment() {
    let coin = coin_from_json().unwrap();
    let priv_bn = unwrap!(random_compressed_key_pair());
    let priv_b0 = unwrap!(random_compressed_key_pair());
    let priv_a0 = unwrap!(random_compressed_key_pair());
    let tx = coin.send_buyer_payment(
        (now_ms() / 1000) as u32,
        &priv_a0.public().to_vec(),
        &priv_b0.public().to_vec(),
        &dhash160(&*priv_bn.private().secret).to_vec(),
        0.001
    ).wait().unwrap();

    let refund_tx = coin.send_seller_spends_buyer_payment(
        tx,
        &priv_b0.private().secret.to_vec(),
        &priv_bn.private().secret.to_vec(),
        0.0999
    ).wait().unwrap();
}

#[test]
fn test_send_and_refund_seller_payment() {
    let coin = coin_from_json().unwrap();
    let priv_bn = unwrap!(random_compressed_key_pair());
    let priv_b0 = unwrap!(random_compressed_key_pair());
    let priv_a0 = unwrap!(random_compressed_key_pair());
    let tx = coin.send_seller_payment(
        (now_ms() / 1000) as u32,
        &priv_a0.public().to_vec(),
        &priv_b0.public().to_vec(),
        &dhash160(&*priv_bn.private().secret).to_vec(),
        0.001
    ).wait().unwrap();

    let refund_tx = coin.send_seller_refunds_payment(
        tx,
        &priv_b0.private().secret.to_vec(),
        0.0999
    ).wait().unwrap();
}

#[test]
fn test_send_and_spend_seller_payment() {
    let coin = coin_from_json().unwrap();
    let priv_bn = unwrap!(random_compressed_key_pair());
    let priv_b0 = unwrap!(random_compressed_key_pair());
    let priv_a0 = unwrap!(random_compressed_key_pair());
    let tx = coin.send_seller_payment(
        (now_ms() / 1000) as u32,
        &priv_a0.public().to_vec(),
        &priv_b0.public().to_vec(),
        &dhash160(&*priv_bn.private().secret).to_vec(),
        0.001
    ).wait().unwrap();

    let refund_tx = coin.send_buyer_spends_seller_payment(
        tx,
        &priv_a0.private().secret.to_vec(),
        &priv_bn.private().secret.to_vec(),
        0.0999
    ).wait().unwrap();
}

#[test]
fn test_wait_for_tx_spend() {
    let coin = coin_from_json().unwrap();
    let coin: Box<UtxoCoinArc> = coin.downcast().unwrap();
    let tx = coin.rpc_client.get_raw_transaction(H256Json::from("1dcf6dfe3672740a0d23c977f8b84bebfdc43b8f797ca050f477b92d1493201a"), 1);
    let tx = tx.wait().unwrap();

    let extended_utxo_tx = Box::new(ExtendedUtxoTx {
        transaction: unwrap!(deserialize((*tx.hex).as_slice())),
        redeem_script: Bytes::from("82ef1f3bc853c46a40ef1ffeebc5bbe60336bce08c2aed2ceee91bed27eaade1"),
    });

    let spent = coin.wait_for_tx_spend(extended_utxo_tx, now_ms() / 1000 + 1000).wait().unwrap();
    println!("Spent found {:?}", spent);
}