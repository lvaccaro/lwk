use std::net::SocketAddr;
use std::sync::Mutex;
use std::{collections::HashMap, io::ErrorKind};

use crate::get_receive_address::{GetReceiveAddressParams, SingleOrMulti, Variant};
use crate::protocol::{
    AuthResult, AuthUserParams, DebugSetMnemonicParams, EntropyParams, EpochParams,
    GetMasterBlindingKeyParams, GetSignatureParams, GetXpubParams, HandshakeCompleteParams,
    HandshakeData, HandshakeInitParams, IsAuthResult, Request, SignMessageParams,
    UpdatePinserverParams, VersionInfoResult,
};
use crate::register_multisig::{
    GetRegisteredMultisigParams, RegisterMultisigParams, RegisteredMultisig,
    RegisteredMultisigDetails,
};
use crate::sign_liquid_tx::{SignLiquidTxParams, TxInputParams};
use crate::{
    derivation_path_to_vec, try_parse_response, vec_to_derivation_path, Error, Network, Result,
};
use connection::Connection;
use elements::bitcoin::bip32::{DerivationPath, Fingerprint, Xpub};
use elements::pset::PartiallySignedTransaction;
use elements_miniscript::slip77::{self, MasterBlindingKey};
use lwk_common::Signer;
use serde::de::DeserializeOwned;
use serde_bytes::ByteBuf;

mod connection;
mod sign_pset;

#[cfg(feature = "serial")]
use crate::consts::{BAUD_RATE, TIMEOUT};

#[derive(Debug)]
pub struct Jade {
    /// Jade working via emulator(tcp), physical(serial/bluetooth)
    conn: Mutex<Connection>,

    /// The network
    pub(crate) network: crate::Network,

    /// Cached xpubs
    cached_xpubs: Mutex<HashMap<DerivationPath, Xpub>>,
}

impl Jade {
    pub fn new(conn: Connection, network: Network) -> Self {
        Self {
            conn: Mutex::new(conn),
            network,
            cached_xpubs: Mutex::new(HashMap::new()),
        }
    }

    pub fn ping(&self) -> Result<u8> {
        self.send(Request::Ping)
    }

    pub fn logout(&self) -> Result<bool> {
        self.send(Request::Logout)
    }

    pub fn version_info(&self) -> Result<VersionInfoResult> {
        self.send(Request::GetVersionInfo)
    }

    pub fn set_epoch(&self, epoch: u64) -> Result<bool> {
        self.send(Request::SetEpoch(EpochParams { epoch }))
    }

    pub fn add_entropy(&self, entropy: Vec<u8>) -> Result<bool> {
        self.send(Request::AddEntropy(EntropyParams { entropy }))
    }

    pub fn auth_user(&self, params: AuthUserParams) -> Result<IsAuthResult<String>> {
        self.send(Request::AuthUser(params))
    }

    pub fn handshake_init(&self, params: HandshakeInitParams) -> Result<AuthResult<HandshakeData>> {
        self.send(Request::HandshakeInit(params))
    }

    pub fn update_pinserver(&self, params: UpdatePinserverParams) -> Result<bool> {
        self.send(Request::UpdatePinserver(params))
    }

    pub fn handshake_complete(&self, params: HandshakeCompleteParams) -> Result<bool> {
        self.send(Request::HandshakeComplete(params))
    }

    fn get_xpub(&self, params: GetXpubParams) -> Result<Xpub> {
        self.send(Request::GetXpub(params))
    }

    pub fn get_receive_address(&self, params: GetReceiveAddressParams) -> Result<String> {
        self.send(Request::GetReceiveAddress(params))
    }

    pub fn get_master_blinding_key(&self, params: GetMasterBlindingKeyParams) -> Result<ByteBuf> {
        self.send(Request::GetMasterBlindingKey(params))
    }

    pub fn sign_message(&self, params: SignMessageParams) -> Result<ByteBuf> {
        self.send(Request::SignMessage(params))
    }

    pub fn get_signature_for_msg(&self, params: GetSignatureParams) -> Result<String> {
        self.send(Request::GetSignature(params))
    }

    pub fn get_signature_for_tx(&self, params: GetSignatureParams) -> Result<ByteBuf> {
        self.send(Request::GetSignature(params))
    }

    pub fn sign_liquid_tx(&self, params: SignLiquidTxParams) -> Result<bool> {
        self.send(Request::SignLiquidTx(params))
    }

    pub fn tx_input(&self, params: TxInputParams) -> Result<ByteBuf> {
        self.send(Request::TxInput(params))
    }

    pub fn debug_set_mnemonic(&self, params: DebugSetMnemonicParams) -> Result<bool> {
        self.send(Request::DebugSetMnemonic(params))
    }

    pub fn register_multisig(&self, params: RegisterMultisigParams) -> Result<bool> {
        self.send(Request::RegisterMultisig(params))
    }

    pub fn get_registered_multisigs(&self) -> Result<HashMap<String, RegisteredMultisig>> {
        self.send(Request::GetRegisteredMultisigs)
    }

    pub fn get_registered_multisig(
        &self,
        params: GetRegisteredMultisigParams,
    ) -> Result<RegisteredMultisigDetails> {
        self.send(Request::GetRegisteredMultisig(params))
    }

    pub fn get_cached_xpub(&self, params: GetXpubParams) -> Result<Xpub> {
        if let Some(xpub) = self
            .cached_xpubs
            .lock()?
            .get(&vec_to_derivation_path(&params.path))
        {
            Ok(*xpub)
        } else {
            self.get_xpub(params)
        }
    }

    pub fn fingerprint(&self) -> Result<Fingerprint> {
        Ok(self.get_master_xpub()?.fingerprint())
    }

    fn check_network(&self, passed: Network) -> Result<()> {
        let init = self.network;
        if passed != init {
            Err(Error::MismatchingXpub { init, passed })
        } else {
            Ok(())
        }
    }

    pub fn get_master_xpub(&self) -> Result<Xpub> {
        let params = GetXpubParams {
            network: self.network,
            path: vec![],
        };
        self.get_cached_xpub(params)
    }

    /// Unlock an already initialized Jade.
    ///
    /// The device asks for the pin,
    /// and the host performs network calls to the pin server
    /// to decrypt the secret on the device.
    pub fn unlock(&self) -> Result<()> {
        match self.auth_user(AuthUserParams::new(self.network))? {
            IsAuthResult::AlreadyAuth(result) => {
                if result {
                    Ok(())
                } else {
                    // Jade is not setup, and the user declined to do it on Jade screen
                    Err(Error::NotInitialized)
                }
            }
            IsAuthResult::AuthResult(result) => {
                let client = reqwest::blocking::Client::new();

                let url = result.urls().first().ok_or(Error::MissingUrlA)?.as_str();
                let resp = client.post(url).send()?;
                let status_code = resp.status().as_u16();
                if status_code != 200 {
                    return Err(Error::HttpStatus(url.to_string(), status_code));
                }

                let params: HandshakeInitParams = serde_json::from_slice(resp.bytes()?.as_ref())?;
                let result = self.handshake_init(params)?;
                let url = result.urls().first().ok_or(Error::MissingUrlA)?.as_str();
                let data = serde_json::to_vec(result.data())?;
                let resp = client.post(url).body(data).send()?;
                let status_code = resp.status().as_u16();
                if status_code != 200 {
                    return Err(Error::HttpStatus(url.to_string(), status_code));
                }
                let params: HandshakeCompleteParams =
                    serde_json::from_slice(resp.bytes()?.as_ref())?;

                let result = self.handshake_complete(params)?;

                if !result {
                    return Err(Error::HandshakeFailed);
                }

                Ok(())
            }
        }
    }

    #[cfg(feature = "serial")]
    pub fn from_serial(
        network: Network,
        port_name: &str,
        timeout: Option<std::time::Duration>,
    ) -> Result<Self> {
        tracing::info!("serial port {port_name}");
        let timeout = timeout.unwrap_or(TIMEOUT);
        let port = serialport::new(port_name, BAUD_RATE)
            .timeout(timeout)
            .open()?;
        Ok(Self::new(port.into(), network))
    }

    #[cfg(feature = "serial")]
    /// Try to unlock a jade on any available serial port, returning all of the attempts
    pub fn from_any_serial(
        network: Network,
        timeout: Option<std::time::Duration>,
    ) -> Vec<Result<Self>> {
        let mut result = vec![];
        let ports = Self::available_ports_with_jade();
        tracing::debug!("available serial ports possibly with jade: {}", ports.len());
        for port in ports {
            let jade_res = Self::from_serial(network, &port.port_name, timeout);
            tracing::debug!("trying: {port:?} return {jade_res:?}");

            // TODO green_qt calls also get_version_info
            result.push(jade_res);
        }
        result
    }

    #[cfg(feature = "serial")]
    pub fn available_ports_with_jade() -> Vec<serialport::SerialPortInfo> {
        let ports = serialport::available_ports().unwrap_or_default();
        tracing::debug!("available serial ports: {}", ports.len());

        ports
            .into_iter()
            .filter(|e| {
                if let serialport::SerialPortType::UsbPort(val) = &e.port_type {
                    crate::JADE_DEVICE_IDS.contains(&(val.vid, val.pid))
                } else {
                    false
                }
            })
            .collect()
    }

    #[cfg(feature = "serial")]
    pub fn from_serial_matching_id(
        network: Network,
        id: &elements::bitcoin::XKeyIdentifier,
        timeout: Option<std::time::Duration>,
    ) -> Option<Self> {
        Self::from_any_serial(network, timeout)
            .into_iter()
            .filter_map(|e| e.ok())
            .find(|e| {
                if let Ok(c) = e.identifier() {
                    &c == id
                } else {
                    false
                }
            })
    }

    pub fn from_socket(socket: SocketAddr, network: Network) -> Result<Self> {
        let stream = std::net::TcpStream::connect(socket)?;
        let conn = Connection::TcpStream(stream);
        Ok(Self::new(conn, network))
    }

    pub fn get_receive_address_single(&self, variant: Variant, path: Vec<u32>) -> Result<String> {
        let params = GetReceiveAddressParams {
            network: self.network,
            address: SingleOrMulti::Single { variant, path },
        };
        self.get_receive_address(params)
    }

    pub fn get_receive_address_multi(&self, name: &str, paths: Vec<Vec<u32>>) -> Result<String> {
        let params = GetReceiveAddressParams {
            network: self.network,
            address: SingleOrMulti::Multi {
                multisig_name: name.to_string(),
                paths,
            },
        };
        self.get_receive_address(params)
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub(crate) fn send<T>(&self, request: Request) -> Result<T>
    where
        T: std::fmt::Debug + DeserializeOwned,
    {
        if let Some(network) = request.network() {
            self.check_network(network)?;
        }
        let buf = request.serialize()?;

        let mut conn = self.conn.lock()?;

        conn.write_all(&buf)?;

        let mut rx = [0u8; 4096];

        let mut total = 0;
        loop {
            match conn.read(&mut rx[total..]) {
                Ok(len) => {
                    total += len;
                    let reader = &rx[..total];

                    if let Some(value) = try_parse_response(reader) {
                        return value;
                    }
                }
                Err(e) => {
                    if e.kind() != ErrorKind::Interrupted {
                        dbg!(&e);
                        return Err(Error::IoError(e));
                    }
                }
            }
        }
    }
}

impl Signer for &Jade {
    type Error = crate::Error;

    fn sign(&self, pset: &mut PartiallySignedTransaction) -> std::result::Result<u32, Self::Error> {
        self.unlock()?;
        Jade::sign(self, pset)
    }

    fn derive_xpub(
        &self,
        path: &elements::bitcoin::bip32::DerivationPath,
    ) -> std::result::Result<Xpub, Self::Error> {
        let params = GetXpubParams {
            network: self.network,
            path: derivation_path_to_vec(path),
        };

        self.unlock()?;
        self.get_cached_xpub(params)
    }

    fn slip77_master_blinding_key(
        &self,
    ) -> std::result::Result<slip77::MasterBlindingKey, Self::Error> {
        let params = GetMasterBlindingKeyParams {
            only_if_silent: false,
        };
        let bytes = self.get_master_blinding_key(params)?;
        let array: [u8; 32] = bytes
            .to_vec()
            .try_into()
            .map_err(|_| Self::Error::Slip77MasterBlindingKeyInvalidSize)?;
        Ok(slip77::MasterBlindingKey::from(array))
    }
}

impl Signer for Jade {
    type Error = crate::Error;

    fn sign(&self, pset: &mut PartiallySignedTransaction) -> std::result::Result<u32, Self::Error> {
        Signer::sign(&self, pset)
    }

    fn derive_xpub(
        &self,
        path: &elements::bitcoin::bip32::DerivationPath,
    ) -> std::result::Result<Xpub, Self::Error> {
        Signer::derive_xpub(&self, path)
    }

    fn slip77_master_blinding_key(&self) -> std::result::Result<MasterBlindingKey, Self::Error> {
        Signer::slip77_master_blinding_key(&self)
    }
}
