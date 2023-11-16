use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use client::Client;
use config::Config;
use jade::get_receive_address::Variant;
use jade::mutex_jade::MutexJade;
use rand::{thread_rng, Rng};
use signer::{AnySigner, SwSigner};
use tiny_jrpc::{tiny_http, JsonRpcServer, Request, Response};
use wollet::bitcoin::bip32::DerivationPath;
use wollet::elements::hex::ToHex;
use wollet::{Wollet, EC};

use crate::model::{ListSignersResponse, ListWalletsResponse, SignerResponse, WalletResponse};

pub mod client;
pub mod config;
pub mod consts;
pub mod error;
pub mod model;

#[derive(Default)]
pub struct State<'a> {
    // TODO: config is read-only, so it's not useful to wrap it in a mutex.
    // Ideally it should be in _another_ struct accessible by method_handler.
    pub config: Config,
    pub wollets: HashMap<String, Wollet>,
    pub signers: HashMap<String, AnySigner<'a>>,
}

pub struct App {
    rpc: Option<JsonRpcServer>,
    config: Config,
}

pub type Result<T> = std::result::Result<T, error::Error>;

impl App {
    pub fn new(config: Config) -> Result<App> {
        tracing::info!("Creating new app with config: {:?}", config);

        Ok(App { rpc: None, config })
    }

    pub fn run(&mut self) -> Result<()> {
        if self.rpc.is_some() {
            return Err(error::Error::AlreadyStarted);
        }
        let state = Arc::new(Mutex::new(State {
            config: self.config.clone(),
            ..Default::default()
        }));
        let server = tiny_http::Server::http(self.config.addr)?;

        let rpc = tiny_jrpc::JsonRpcServer::new(server, state, method_handler);
        self.rpc = Some(rpc);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        match self.rpc.as_ref() {
            Some(rpc) => {
                rpc.stop();
                Ok(())
            }
            None => Err(error::Error::NotStarted),
        }
    }

    pub fn is_running(&self) -> Result<bool> {
        match self.rpc.as_ref() {
            Some(rpc) => Ok(rpc.is_running()),
            None => Err(error::Error::NotStarted),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.config.addr
    }

    pub fn join_threads(&mut self) -> Result<()> {
        self.rpc
            .take()
            .ok_or(error::Error::NotStarted)?
            .join_threads();
        Ok(())
    }

    pub fn client(&self) -> Result<Client> {
        Client::new(self.config.addr)
    }
}

fn method_handler(request: Request, state: Arc<Mutex<State>>) -> tiny_jrpc::Result<Response> {
    tracing::debug!(
        "method: {} params: {:?} ",
        request.method.as_str(),
        request.params
    );
    let response = match request.method.as_str() {
        "generate_signer" => {
            let (_signer, mnemonic) = SwSigner::random(&EC)?;
            Response::result(
                request.id,
                serde_json::to_value(model::GenerateSignerResponse {
                    mnemonic: mnemonic.to_string(),
                })?,
            )
        }
        "version" => Response::result(
            request.id,
            serde_json::to_value(model::VersionResponse {
                version: consts::APP_VERSION.into(),
            })?,
        ),
        "load_wallet" => {
            let r: model::LoadWalletRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            if s.wollets.contains_key(&r.name) {
                return Err(tiny_jrpc::error::Error::WalletAlreadyLoaded(r.name));
            }
            // TODO recognize different name same descriptor?
            let wollet = Wollet::new(
                s.config.network.clone(),
                &s.config.electrum_url,
                s.config.tls,
                s.config.validate_domain,
                &s.config.datadir,
                &r.descriptor,
            )?;

            let a = |w: &Wollet| w.address(Some(0)).unwrap().address().to_string();

            let vec: Vec<_> = s
                .wollets
                .iter()
                .filter(|(_, w)| a(w) == a(&wollet))
                .map(|(n, _)| n)
                .collect();
            if let Some(existing) = vec.first() {
                // TODO: maybe a different error more clear?
                return Err(tiny_jrpc::error::Error::WalletAlreadyLoaded(
                    existing.to_string(),
                ));
            }

            s.wollets.insert(r.name.clone(), wollet);
            Response::result(
                request.id,
                serde_json::to_value(model::WalletResponse {
                    descriptor: r.descriptor,
                    name: r.name,
                })?,
            )
        }
        "unload_wallet" => {
            let r: model::UnloadWalletRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            match s.wollets.remove(&r.name) {
                Some(removed) => Response::result(
                    request.id,
                    serde_json::to_value(model::UnloadWalletResponse {
                        unloaded: WalletResponse {
                            name: r.name,
                            descriptor: removed.descriptor().to_string(),
                        },
                    })?,
                ),
                None => {
                    return Err(tiny_jrpc::error::Error::WalletNotExist(r.name));
                }
            }
        }
        "list_wallets" => {
            let s = state.lock().unwrap();
            let wallets: Vec<_> = s
                .wollets
                .iter()
                .map(|(name, wollet)| WalletResponse {
                    descriptor: wollet.descriptor().to_string(),
                    name: name.clone(),
                })
                .collect();
            let r = ListWalletsResponse { wallets };
            Response::result(request.id, serde_json::to_value(r)?)
        }
        "load_signer" => {
            let r: model::LoadSignerRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();

            if s.signers.contains_key(&r.name) {
                return Err(tiny_jrpc::error::Error::SignerAlreadyLoaded(r.name));
            }

            let signer = match r.kind.as_str() {
                "software" => {
                    if r.mnemonic.is_none() {
                        return Err(tiny_jrpc::error::Error::Generic(
                            "Mnemonic must be set for software signer".to_string(),
                        ));
                    }
                    let mnemonic = r.mnemonic.unwrap();
                    AnySigner::Software(SwSigner::new(&mnemonic, &EC)?)
                }
                "serial" => {
                    let network = s.config.jade_network();
                    let mut jade = MutexJade::from_serial(network)?;
                    // TODO: move conditional unlocking to jade
                    let jade_state = jade.get_mut().unwrap().version_info().unwrap().jade_state;
                    if jade_state == jade::protocol::JadeState::Locked {
                        jade.unlock().unwrap();
                    }
                    AnySigner::Jade(jade)
                }
                _ => {
                    return Err(tiny_jrpc::error::Error::Generic(
                        "Invalid signer kind".to_string(),
                    ));
                }
            };

            let vec: Vec<_> = s
                .signers
                .iter()
                .filter(|(_, s)| s.id().unwrap() == signer.id().unwrap())
                .map(|(n, _)| n)
                .collect();
            if let Some(existing) = vec.first() {
                // TODO: maybe a different error more clear?
                return Err(tiny_jrpc::error::Error::SignerAlreadyLoaded(
                    existing.to_string(),
                ));
            }

            let resp: SignerResponse = (r.name.clone(), &signer).try_into()?;

            s.signers.insert(r.name, signer);
            Response::result(request.id, serde_json::to_value(resp)?)
        }
        "unload_signer" => {
            let r: model::UnloadSignerRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            match s.signers.remove(&r.name) {
                Some(removed) => {
                    let signer: SignerResponse = (r.name.clone(), &removed).try_into()?;
                    Response::result(
                        request.id,
                        serde_json::to_value(model::UnloadSignerResponse { unloaded: signer })?,
                    )
                }
                None => {
                    return Err(tiny_jrpc::error::Error::SignerNotExist(r.name));
                }
            }
        }
        "list_signers" => {
            let s = state.lock().unwrap();
            let signers: Vec<_> = s
                .signers
                .iter()
                .map(|(name, signer)| (name.clone(), signer).try_into().unwrap()) // TODO
                .collect();
            let r = ListSignersResponse { signers };
            Response::result(request.id, serde_json::to_value(r)?)
        }
        "address" => {
            let r: model::AddressRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let wollet = s
                .wollets
                .get_mut(&r.name)
                .ok_or_else(|| tiny_jrpc::error::Error::WalletNotExist(r.name.clone()))?;
            wollet.sync_txs()?; // To update the last unused index
            let addr = wollet.address(r.index)?;
            Response::result(
                request.id,
                serde_json::to_value(model::AddressResponse {
                    address: addr.address().clone(),
                    index: addr.index(),
                })?,
            )
        }
        "balance" => {
            let r: model::BalanceRequest =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let wollet = s
                .wollets
                .get_mut(&r.name)
                .ok_or_else(|| tiny_jrpc::error::Error::WalletNotExist(r.name.clone()))?;
            wollet.sync_txs()?;
            let balance = wollet.balance()?;
            Response::result(
                request.id,
                serde_json::to_value(model::BalanceResponse { balance })?,
            )
        }
        "send_many" => {
            let r: model::SendRequest = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s
                .wollets
                .get_mut(&r.name)
                .ok_or_else(|| tiny_jrpc::error::Error::WalletNotExist(r.name.clone()))?;
            wollet.sync_txs()?;
            let tx = wollet.send_many(r.addressees, r.fee_rate)?;
            Response::result(
                request.id,
                serde_json::to_value(model::SendResponse {
                    pset: tx.to_string(),
                })?,
            )
        }
        "singlesig_descriptor" => {
            let r: model::SinglesigDescriptorRequest =
                serde_json::from_value(request.params.unwrap())?;
            let s = state.lock().unwrap();

            let signer = s
                .signers
                .get(&r.name)
                .ok_or_else(|| tiny_jrpc::error::Error::SignerNotExist(r.name.to_string()))?;

            let variant = match r.singlesig_kind.as_str() {
                "wpkh" => Variant::Wpkh,
                "shwpkh" => Variant::ShWpkh,
                v => {
                    return Err(tiny_jrpc::error::Error::Generic(format!(
                        "invalid variant {}",
                        v
                    )))
                }
            };

            if r.descriptor_blinding_key != "slip77" {
                return Err(tiny_jrpc::error::Error::Generic(format!(
                    "invalid or not yet implemented descriptor_blinding_key {}",
                    r.descriptor_blinding_key
                )));
            }

            let descriptor = singlesig_desc(signer, variant);
            Response::result(
                request.id,
                serde_json::to_value(model::SinglesigDescriptorResponse { descriptor })?,
            )
        }
        "stop" => {
            return Err(tiny_jrpc::error::Error::Stop);
        }
        _ => Response::unimplemented(request.id),
    };
    Ok(response)
}

// TODO the following is duplicated from test_session:
// 1) rename crate pset_common -> common
// 2) move things that must be trasversaly used but that not depend on anything in this workspace there
//    like the following function if refactored taking xpub and fingerprint insteaf of singer
fn singlesig_desc(signer: &AnySigner, variant: Variant) -> String {
    let (prefix, path, suffix) = match variant {
        Variant::Wpkh => ("elwpkh", "84h/1h/0h", ""),
        Variant::ShWpkh => ("elsh(wpkh", "49h/1h/0h", ")"),
    };
    let fingerprint = signer.fingerprint().unwrap();
    let xpub = signer
        .derive_xpub(&DerivationPath::from_str(&format!("m/{path}")).unwrap())
        .unwrap();

    let slip77_key = generate_slip77(); // TODO derive from mnemonic instead

    // m / purpose' / coin_type' / account' / change / address_index
    format!("ct(slip77({slip77_key}),{prefix}([{fingerprint}/{path}]{xpub}/<0;1>/*){suffix})")
}

pub fn generate_slip77() -> String {
    let mut bytes = [0u8; 32];
    thread_rng().fill(&mut bytes);
    bytes.to_hex()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn app_random_port() -> App {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let config = Config {
            addr,
            ..Default::default()
        };
        let mut app = App::new(config).unwrap();
        app.run().unwrap();
        app
    }

    #[test]
    fn version() {
        let mut app = app_random_port();
        let addr = app.addr();
        let url = addr.to_string();
        dbg!(&url);

        let client = jsonrpc::Client::simple_http(&url, None, None).unwrap();
        let request = client.build_request("version", None);
        let response = client.send_request(request).unwrap();

        let result = response.result.unwrap().to_string();
        let actual: model::VersionResponse = serde_json::from_str(&result).unwrap();
        assert_eq!(actual.version, consts::APP_VERSION);

        app.stop().unwrap();
        app.join_threads().unwrap();
    }
}
