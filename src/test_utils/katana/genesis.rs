use std::collections::HashMap;
use std::fs;
use std::marker::PhantomData;
use std::path::PathBuf;

use cairo_lang_starknet::casm_contract_class::CasmContractClass;
use cairo_lang_starknet::contract_class::ContractClass;
use ethers::signers::LocalWallet;
use ethers::signers::Signer;
use ethers::types::U256;
use eyre::{eyre, Result};
use katana_primitives::block::GasPrices;
use katana_primitives::contract::{StorageKey, StorageValue};
use katana_primitives::genesis::constant::DEFAULT_FEE_TOKEN_ADDRESS;
use katana_primitives::genesis::json::{FeeTokenConfigJson, GenesisJson};
use katana_primitives::{
    contract::ContractAddress,
    genesis::json::{GenesisClassJson, GenesisContractJson, PathOrFullArtifact},
};
use lazy_static::lazy_static;
use rayon::prelude::*;
use reth_primitives::B256;
use serde::Serialize;
use serde_json::Value;
use serde_with::serde_as;
use starknet::core::serde::unsigned_field_element::UfeHex;
use starknet::core::types::contract::legacy::LegacyContractClass;
use starknet::core::types::FieldElement;
use starknet::core::utils::{get_contract_address, get_storage_var_address, get_udc_deployed_address, UdcUniqueness};
use walkdir::WalkDir;

lazy_static! {
    static ref SALT: FieldElement = FieldElement::from_bytes_be(&[0u8; 32]).unwrap();
}

#[serde_as]
#[derive(Serialize)]
pub struct Hex(#[serde_as(as = "UfeHex")] pub FieldElement);

#[derive(Serialize)]
pub struct KatanaManifest {
    pub declarations: HashMap<String, Hex>,
    pub deployments: HashMap<String, Hex>,
}

#[derive(Debug, Clone)]
pub struct Uninitialized;
#[derive(Debug, Clone)]
pub struct Loaded;
#[derive(Debug, Clone)]
pub struct Initialized;

#[derive(Debug, Clone)]
pub struct KatanaGenesisBuilder<T> {
    coinbase: FieldElement,
    classes: Vec<GenesisClassJson>,
    class_hashes: HashMap<String, FieldElement>,
    contracts: HashMap<ContractAddress, GenesisContractJson>,
    fee_token_storage: HashMap<StorageKey, StorageValue>,
    cache: HashMap<String, FieldElement>,
    status: PhantomData<T>,
}

impl<T> KatanaGenesisBuilder<T> {
    pub fn update_state<State>(self) -> KatanaGenesisBuilder<State> {
        KatanaGenesisBuilder {
            coinbase: self.coinbase,
            classes: self.classes,
            class_hashes: self.class_hashes,
            contracts: self.contracts,
            fee_token_storage: self.fee_token_storage,
            cache: self.cache,
            status: PhantomData::<State>,
        }
    }

    fn kakarot_class_hash(&self) -> Result<FieldElement> {
        self.class_hashes.get("kakarot").cloned().ok_or(eyre!("Missing Kakarot class hash"))
    }

    pub fn contract_account_class_hash(&self) -> Result<FieldElement> {
        self.class_hashes.get("contract_account").cloned().ok_or(eyre!("Missing contract account class hash"))
    }

    pub fn eoa_class_hash(&self) -> Result<FieldElement> {
        self.class_hashes.get("externally_owned_account").cloned().ok_or(eyre!("Missing eoa account class hash"))
    }

    pub fn proxy_class_hash(&self) -> Result<FieldElement> {
        self.class_hashes.get("proxy").cloned().ok_or(eyre!("Missing proxy class hash"))
    }

    pub fn precompiles_class_hash(&self) -> Result<FieldElement> {
        self.class_hashes.get("precompiles").cloned().ok_or(eyre!("Missing precompiles class hash"))
    }
}

impl Default for KatanaGenesisBuilder<Uninitialized> {
    fn default() -> Self {
        KatanaGenesisBuilder {
            coinbase: FieldElement::ZERO,
            classes: vec![],
            class_hashes: HashMap::new(),
            contracts: HashMap::new(),
            fee_token_storage: HashMap::new(),
            cache: HashMap::new(),
            status: PhantomData::<Uninitialized>,
        }
    }
}

impl KatanaGenesisBuilder<Uninitialized> {
    /// Load the classes from the given path. Computes the class hashes and stores them in the builder.
    #[must_use]
    pub fn load_classes(mut self, path: PathBuf) -> KatanaGenesisBuilder<Loaded> {
        let entries = WalkDir::new(path).into_iter().filter(|e| e.is_ok() && e.as_ref().unwrap().file_type().is_file());
        let classes = entries
            .par_bridge()
            .map(|entry| {
                let path = entry.unwrap().path().to_path_buf();
                let artifact = fs::read_to_string(&path).expect("Failed to read artifact");
                (
                    path,
                    GenesisClassJson {
                        class: PathOrFullArtifact::Artifact(
                            serde_json::from_str(&artifact).expect("Failed to parse artifact"),
                        ),
                        class_hash: None,
                    },
                )
            })
            .collect::<Vec<_>>();

        self.class_hashes = classes
            .par_iter()
            .filter_map(|(path, class)| {
                let artifact = match &class.class {
                    PathOrFullArtifact::Artifact(artifact) => artifact,
                    PathOrFullArtifact::Path(_) => unreachable!("Expected artifact"),
                };
                let class_hash = compute_class_hash(artifact).ok()?;
                Some((path.file_stem().unwrap().to_str().unwrap().to_string(), class_hash))
            })
            .collect::<HashMap<_, _>>();
        self.classes = classes.into_iter().map(|(_, class)| class).collect();

        self.update_state()
    }
}

impl KatanaGenesisBuilder<Loaded> {
    /// Add the Kakarot contract to the genesis. Updates the state to [Initialized].
    /// Once in the [Initialized] status, the builder can be built.
    pub fn with_kakarot(mut self, coinbase_address: FieldElement) -> Result<KatanaGenesisBuilder<Initialized>> {
        let kakarot_class_hash = self.kakarot_class_hash()?;

        let contract_account_class_hash = self.contract_account_class_hash()?;
        let eoa_class_hash = self.eoa_class_hash()?;
        let proxy_class_hash = self.proxy_class_hash()?;
        let precompiles_class_hash = self.precompiles_class_hash()?;

        // Construct the kakarot contract address. Based on the constructor args from
        // https://github.com/kkrt-labs/kakarot/blob/main/src/kakarot/kakarot.cairo#L23
        let kakarot_address = ContractAddress::new(get_udc_deployed_address(
            *SALT,
            kakarot_class_hash,
            &UdcUniqueness::NotUnique,
            &[
                FieldElement::ZERO,
                DEFAULT_FEE_TOKEN_ADDRESS.0,
                contract_account_class_hash,
                eoa_class_hash,
                proxy_class_hash,
                precompiles_class_hash,
            ],
        ));
        // Cache the address for later use.
        self.cache.insert("kakarot_address".to_string(), kakarot_address.0);

        // Construct the kakarot contract storage.
        let kakarot_storage = [
            (storage_addr("native_token_address")?, *DEFAULT_FEE_TOKEN_ADDRESS),
            (storage_addr("contract_account_class_hash")?, contract_account_class_hash),
            (storage_addr("externally_owned_account_class_hash")?, eoa_class_hash),
            (storage_addr("account_proxy_class_hash")?, proxy_class_hash),
            (storage_addr("precompiles_class_hash")?, precompiles_class_hash),
            (storage_addr("coinbase")?, coinbase_address),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();

        let kakarot = GenesisContractJson {
            class: kakarot_class_hash,
            balance: None,
            nonce: None,
            storage: Some(kakarot_storage),
        };

        self.contracts.insert(kakarot_address, kakarot);
        self.coinbase = coinbase_address;

        Ok(self.update_state())
    }
}

impl KatanaGenesisBuilder<Initialized> {
    /// Add an EOA to the genesis. The EOA is deployed to the address derived from the given private key.
    pub fn with_eoa(mut self, private_key: B256) -> Result<Self> {
        let evm_address = self.evm_address(private_key)?;

        let kakarot_address = self.cache_load("kakarot_address")?;
        let eoa_class_hash = self.eoa_class_hash()?;
        let proxy_class_hash = self.proxy_class_hash()?;

        // Set the eoa storage
        let eoa_storage = [
            (storage_addr("evm_address")?, evm_address),
            (storage_addr("kakarot_address")?, kakarot_address),
            (storage_addr("_implementation")?, eoa_class_hash),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();

        let eoa =
            GenesisContractJson { class: proxy_class_hash, balance: None, nonce: None, storage: Some(eoa_storage) };

        let starknet_address = self.compute_starknet_address(evm_address)?;
        self.contracts.insert(starknet_address, eoa);

        // Set the allowance for the EOA to the Kakarot contract.
        let key = get_storage_var_address("ERC20_allowances", &[*starknet_address, kakarot_address])?;
        let storage =
            [(key, FieldElement::from(u128::MAX)), (key + 1u8.into(), FieldElement::from(u128::MAX))].into_iter();
        self.fee_token_storage.extend(storage);

        // Write the address to the Kakarot evm to starknet mapping
        let kakarot_address = ContractAddress::new(kakarot_address);
        let kakarot_contract = self.contracts.get_mut(&kakarot_address).ok_or(eyre!("Kakarot contract missing"))?;
        kakarot_contract
            .storage
            .get_or_insert_with(HashMap::new)
            .extend([(get_storage_var_address("evm_to_starknet_address", &[evm_address])?, starknet_address.0)]);

        Ok(self)
    }

    /// Fund the starknet address deployed for the evm address of the passed private key
    /// with the given amount of tokens.
    pub fn fund(mut self, pk: B256, amount: U256) -> Result<Self> {
        let evm_address = self.evm_address(pk)?;
        let starknet_address = self.compute_starknet_address(evm_address)?;
        let eoa = self.contracts.get_mut(&starknet_address).ok_or(eyre!("Missing EOA contract"))?;

        let key = get_storage_var_address("ERC20_balances", &[*starknet_address])?;
        let low = amount & U256::from(u128::MAX);
        let low: u128 = low.try_into().unwrap(); // safe to unwrap
        let high = amount >> U256::from(128);
        let high: u128 = high.try_into().unwrap(); // safe to unwrap

        let storage = [(key, FieldElement::from(low)), (key + 1u8.into(), FieldElement::from(high))].into_iter();
        self.fee_token_storage.extend(storage);

        eoa.balance = Some(amount);

        Ok(self)
    }

    /// Consume the [KatanaGenesisBuilder] and returns the corresponding [GenesisJson].
    pub fn build(self) -> Result<GenesisJson> {
        Ok(GenesisJson {
            parent_hash: FieldElement::ZERO,
            state_root: FieldElement::ZERO,
            number: 0,
            timestamp: 0,
            sequencer_address: self.compute_starknet_address(self.coinbase)?,
            gas_prices: GasPrices::default(),
            classes: self.classes,
            fee_token: FeeTokenConfigJson {
                name: "Ether".to_string(),
                symbol: "ETH".to_string(),
                decimals: 18,
                storage: Some(self.fee_token_storage),
                address: None,
                class: None,
            },
            universal_deployer: None,
            accounts: HashMap::new(),
            contracts: self.contracts,
        })
    }

    /// Returns the manifest of the genesis.
    pub fn manifest(&self) -> KatanaManifest {
        let cache = self.cache().clone().into_iter().map(|(k, v)| (k, Hex(v))).collect::<HashMap<_, _>>();
        let class_hashes = self.class_hashes().clone().into_iter().map(|(k, v)| (k, Hex(v))).collect::<HashMap<_, _>>();
        KatanaManifest { declarations: class_hashes, deployments: cache }
    }

    /// Compute the Starknet address for the given Ethereum address.
    pub fn compute_starknet_address(&self, evm_address: FieldElement) -> Result<ContractAddress> {
        let kakarot_address = self.cache_load("kakarot_address")?;
        let proxy_class_hash = self.proxy_class_hash()?;

        Ok(ContractAddress::new(get_contract_address(evm_address, proxy_class_hash, &[], kakarot_address)))
    }

    fn evm_address(&self, pk: B256) -> Result<FieldElement> {
        let wallet = LocalWallet::from_bytes(pk.as_slice())?;
        let evm_address = wallet.address();
        Ok(FieldElement::from_byte_slice_be(evm_address.as_bytes())?)
    }

    pub fn cache_load(&self, key: &str) -> Result<FieldElement> {
        self.cache.get(key).cloned().ok_or(eyre!("Cache miss for {key} address"))
    }

    pub fn cache(&self) -> &HashMap<String, FieldElement> {
        &self.cache
    }

    pub fn class_hashes(&self) -> &HashMap<String, FieldElement> {
        &self.class_hashes
    }
}

fn compute_class_hash(class: &Value) -> Result<FieldElement> {
    match serde_json::from_value::<ContractClass>(class.clone()) {
        Ok(casm) => {
            let casm = CasmContractClass::from_contract_class(casm, true).expect("Failed to convert class");
            Ok(FieldElement::from_bytes_be(&casm.compiled_class_hash().to_be_bytes())?)
        }
        Err(_) => {
            let casm: LegacyContractClass =
                serde_json::from_value(class.clone()).expect("Failed to parse class code v0");
            Ok(casm.class_hash()?)
        }
    }
}

fn storage_addr(var_name: &str) -> Result<FieldElement> {
    Ok(get_storage_var_address(var_name, &[])?)
}