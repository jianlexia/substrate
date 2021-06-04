// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `Structopt`-ready structs for `try-runtime`.

use parity_scale_codec::{Decode, Encode};
use std::{fmt::Debug, path::PathBuf, str::FromStr, sync::Arc};
use sc_service::Configuration;
use sc_cli::{CliConfiguration, ExecutionStrategy, WasmExecutionMethod};
use sc_executor::NativeExecutor;
use sc_service::NativeExecutionDispatch;
use sp_state_machine::StateMachine;
use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_core::{
	offchain::{
		OffchainWorkerExt, OffchainDbExt, TransactionPoolExt,
		testing::{TestOffchainExt, TestTransactionPoolExt}
	},
	storage::{StorageData, StorageKey, well_known_keys},
};
use sp_keystore::{KeystoreExt, testing::KeyStore};
use remote_externalities::{Builder, Mode, SnapshotConfig, OfflineConfig, OnlineConfig, rpc_api};

#[derive(Debug, Clone, structopt::StructOpt)]
pub enum Command {
	OnRuntimeUpgrade(OnRuntimeUpgradeCmd),
	OffchainWorker(OffchainWorkerCmd),
}

#[derive(Debug, Clone, structopt::StructOpt)]
pub struct OnRuntimeUpgradeCmd {
	#[structopt(subcommand)]
	pub state: State,
}

#[derive(Debug, Clone, structopt::StructOpt)]
pub struct OffchainWorkerCmd {
	/// Hash of the block whose header to use to execute the offchain worker.
	#[structopt(short, long, multiple = false, parse(try_from_str = parse::hash))]
	pub header_at: String,

	#[structopt(subcommand)]
	pub state: State,

	/// Wether or not to overwrite the code from state with the code from
	/// the specified chain spec.
	#[structopt(long)]
	pub overwrite_code: bool,
}

#[derive(Debug, Clone, structopt::StructOpt)]
pub struct SharedParams {
	/// The shared parameters
	#[allow(missing_docs)]
	#[structopt(flatten)]
	pub shared_params: sc_cli::SharedParams,

	/// The execution strategy that should be used for benchmarks
	#[structopt(
		long = "execution",
		value_name = "STRATEGY",
		possible_values = &ExecutionStrategy::variants(),
		case_insensitive = true,
		default_value = "Native",
	)]
	pub execution: ExecutionStrategy,

	/// Method for executing Wasm runtime code.
	#[structopt(
		long = "wasm-execution",
		value_name = "METHOD",
		possible_values = &WasmExecutionMethod::variants(),
		case_insensitive = true,
		default_value = "compiled"
	)]
	pub wasm_method: WasmExecutionMethod,

	/// The number of 64KB pages to allocate for Wasm execution. Defaults to
	/// sc_service::Configuration.default_heap_pages.
	#[structopt(long)]
	pub heap_pages: Option<u64>,
}

/// Various commands to try out against runtime state at a specific block.
#[derive(Debug, Clone, structopt::StructOpt)]
pub struct TryRuntimeCmd {
	#[structopt(flatten)]
	pub shared: SharedParams,

	#[structopt(subcommand)]
	pub command: Command,
}

/// The source of runtime state to try operations against.
#[derive(Debug, Clone, structopt::StructOpt)]
pub enum State {
	/// Use a state snapshot as the source of runtime state. NOTE: for the offchain-worker command this
	/// is only partially supported at the moment and you must have a relevant archive node exposed on
	//// localhost:9944 in order to query the block header.
	Snap {
		snapshot_path: PathBuf,
	},

	/// Use a live chain as the source of runtime state.
	Live {
		/// An optional state snapshot file to WRITE to. Not written if set to `None`.
		#[structopt(short, long)]
		snapshot_path: Option<PathBuf>,

		/// The block hash at which to connect.
		/// Will be latest finalized head if not provided.
		#[structopt(short, long, multiple = false, parse(try_from_str = parse::hash))]
		block_at: Option<String>,

		/// The modules to scrape. If empty, entire chain state will be scraped.
		#[structopt(short, long, require_delimiter = true)]
		modules: Option<Vec<String>>,

		/// The url to connect to.
		#[structopt(default_value = "ws://localhost:9944", parse(try_from_str = parse::url))]
		url: String,
	},
}

async fn on_runtime_upgrade<B, ExecDispatch>(
	shared: SharedParams,
	command: OnRuntimeUpgradeCmd,
	config: Configuration
) -> sc_cli::Result<()>
where
	B: BlockT,
	B::Hash: FromStr,
	<B::Hash as FromStr>::Err: Debug,
	NumberFor<B>: FromStr,
	<NumberFor<B> as FromStr>::Err: Debug,
	ExecDispatch: NativeExecutionDispatch + 'static,
{
	let spec = config.chain_spec;
	let genesis_storage = spec.build_storage()?;

	let code = StorageData(
		genesis_storage
			.top
			.get(well_known_keys::CODE)
			.expect("code key must exist in genesis storage; qed")
			.to_vec(),
	);
	let code_key = StorageKey(well_known_keys::CODE.to_vec());

	let wasm_method = shared.wasm_method;
	let execution = shared.execution;
	let heap_pages = if shared.heap_pages.is_some() {
		shared.heap_pages
	} else {
		config.default_heap_pages
	};

	let mut changes = Default::default();
	let max_runtime_instances = config.max_runtime_instances;
	let executor = NativeExecutor::<ExecDispatch>::new(
		wasm_method.into(),
		heap_pages,
		max_runtime_instances,
	);

	let ext = {
		let builder = match command.state {
			State::Snap { snapshot_path } => {
				Builder::<B>::new().mode(Mode::Offline(OfflineConfig {
					state_snapshot: SnapshotConfig::new(snapshot_path),
				}))
			},
			State::Live {
				url,
				snapshot_path,
				block_at,
				modules
			} => Builder::<B>::new().mode(Mode::Online(OnlineConfig {
				transport: url.to_owned().into(),
				state_snapshot: snapshot_path.as_ref().map(SnapshotConfig::new),
				modules: modules.to_owned().unwrap_or_default(),
				at: block_at.as_ref()
					.map(|b| b.parse().map_err(|e| format!("Could not parse hash: {:?}", e))).transpose()?,
				..Default::default()
			})),
		};

		// inject the code into this ext.
		builder.inject(&[(code_key, code)]).build().await?
	};

	let encoded_result = StateMachine::<_, _, NumberFor<B>, _>::new(
		&ext.backend,
		None,
		&mut changes,
		&executor,
		"TryRuntime_on_runtime_upgrade",
		&[],
		ext.extensions,
		&sp_state_machine::backend::BackendRuntimeCode::new(&ext.backend)
			.runtime_code()?,
		sp_core::testing::TaskExecutor::new(),
	)
	.execute(execution.into())
	.map_err(|e| format!("failed to execute 'TryRuntime_on_runtime_upgrade' due to {:?}", e))?;

	let (weight, total_weight) = <(u64, u64) as Decode>::decode(&mut &*encoded_result)
		.map_err(|e| format!("failed to decode output due to {:?}", e))?;
	log::info!(
		"TryRuntime_on_runtime_upgrade executed without errors. Consumed weight = {}, total weight = {} ({})",
		weight,
		total_weight,
		weight as f64 / total_weight as f64
	);

	Ok(())
}

async fn offchain_worker<B, ExecDispatch>(
	shared: SharedParams,
	command: OffchainWorkerCmd,
	config: Configuration,
)-> sc_cli::Result<()>
where
	B: BlockT,
	B::Hash: FromStr,
	B::Header: serde::de::DeserializeOwned,
	<B::Hash as FromStr>::Err: Debug,
	NumberFor<B>: FromStr,
	<NumberFor<B> as FromStr>::Err: Debug,
	ExecDispatch: NativeExecutionDispatch + 'static,
{
	let wasm_method = shared.wasm_method;
	let execution = shared.execution;
	let heap_pages = if shared.heap_pages.is_some() {
		shared.heap_pages
	} else {
		config.default_heap_pages
	};

	let mut changes = Default::default();
	let max_runtime_instances = config.max_runtime_instances;
	let executor = NativeExecutor::<ExecDispatch>::new(
		wasm_method.into(),
		heap_pages,
		max_runtime_instances,
	);

	let (mode, url) = match command.state {
			State::Live {
				url,
				snapshot_path,
				block_at,
				modules
			} => {
				let online_config = OnlineConfig {
					transport: url.to_owned().into(),
					state_snapshot: snapshot_path.as_ref().map(SnapshotConfig::new),
					modules: modules.to_owned().unwrap_or_default(),
					at: block_at.as_ref()
						.map(|b| b.parse().map_err(|e| format!("Could not parse hash: {:?}", e))).transpose()?,
					..Default::default()
				};

				(Mode::Online(online_config), url)
			},
			State::Snap { snapshot_path } => {
				// This is a temporary hack; the url is used just to get the header. We should try
				// and get the header out of state, OR use an arbitrary header if thats ok, OR allow
				// the user to feed in a header via file.
				// This assumes you have a node running on local host default
				let url = "ws://127.0.0.1:9944".to_string();
				let mode = Mode::Offline(OfflineConfig {
					state_snapshot: SnapshotConfig::new(snapshot_path),
				});

				(mode, url)
			}
	};
	let builder = Builder::<B>::new().mode(mode);
	let mut ext = if command.overwrite_code {
		// get the code to inject inject into the ext
		let spec = config.chain_spec; // TODO DRY this code block
		let genesis_storage = spec.build_storage()?;
		let code = StorageData(
			genesis_storage
				.top
				.get(well_known_keys::CODE)
				.expect("code key must exist in genesis storage; qed")
				.to_vec(),
		);
		let code_key = StorageKey(well_known_keys::CODE.to_vec());
		builder.inject(&[(code_key, code)]).build().await?
	} else {
		builder.build().await?
	};

	// register externality extensions in order to provide host interface for OCW to the runtime
	let (offchain, _offchain_state) = TestOffchainExt::new();
	let (pool, _pool_state) = TestTransactionPoolExt::new();
	ext.register_extension(OffchainDbExt::new(offchain.clone()));
	ext.register_extension(OffchainWorkerExt::new(offchain));
	ext.register_extension(KeystoreExt(Arc::new(KeyStore::new())));
	ext.register_extension(TransactionPoolExt::new(pool));

	let header_hash: B::Hash = command.header_at.parse().unwrap();
	let header = rpc_api::get_header::<B, _>(url,header_hash).await;

	let _ = StateMachine::<_, _, NumberFor<B>, _>::new(
		&ext.backend,
		None,
		&mut changes,
		&executor,
		"OffchainWorkerApi_offchain_worker",
		header.encode().as_ref(),
		ext.extensions,
		&sp_state_machine::backend::BackendRuntimeCode::new(&ext.backend)
			.runtime_code()?,
		sp_core::testing::TaskExecutor::new(),
	)
	.execute(execution.into())
	.map_err(|e| format!("failed to execute 'OffchainWorkerApi_offchain_worker' due to {:?}", e))?;

	log::info!("OffchainWorkerApi_offchain_worker executed without errors.");

	Ok(())
}

impl TryRuntimeCmd {
	pub async fn run<B, ExecDispatch>(&self, config: Configuration) -> sc_cli::Result<()>
	where
		B: BlockT,
		B::Header: serde::de::DeserializeOwned,
		B::Hash: FromStr,
		<B::Hash as FromStr>::Err: Debug,
		NumberFor<B>: FromStr,
		<NumberFor<B> as FromStr>::Err: Debug,
		ExecDispatch: NativeExecutionDispatch + 'static,
	{
		match &self.command {
			Command::OnRuntimeUpgrade(ref cmd) => {
				on_runtime_upgrade::<B, ExecDispatch>(self.shared.clone(), cmd.clone(), config).await
			}
			Command::OffchainWorker(cmd) => {
				offchain_worker::<B, ExecDispatch>(self.shared.clone(), cmd.clone(), config).await
			}
		}
	}
}

impl CliConfiguration for TryRuntimeCmd {
	fn shared_params(&self) -> &sc_cli::SharedParams {
		&self.shared.shared_params
	}

	fn chain_id(&self, _is_dev: bool) -> sc_cli::Result<String> {
		Ok(match self.shared.shared_params.chain {
			Some(ref chain) => chain.clone(),
			None => "dev".into(),
		})
	}
}

// Utils for parsing user input
mod parse {
	pub fn hash(block_number: &str) -> Result<String, String> {
		let block_number = if block_number.starts_with("0x") {
			&block_number[2..]
		} else {
			block_number
		};

		if let Some(pos) = block_number.chars().position(|c| !c.is_ascii_hexdigit()) {
			Err(format!(
				"Expected block hash, found illegal hex character at position: {}",
				2 + pos,
			))
		} else {
			Ok(block_number.into())
		}
	}

	pub fn url(s: &str) -> Result<String, &'static str> {
		if s.starts_with("ws://") || s.starts_with("wss://") {
			// could use Url crate as well, but lets keep it simple for now.
			Ok(s.to_string())
		} else {
			Err("not a valid WS(S) url: must start with 'ws://' or 'wss://'")
		}
	}
}

