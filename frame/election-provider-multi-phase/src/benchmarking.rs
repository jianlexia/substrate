// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
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

//! Two phase election pallet benchmarking.

use super::*;
use crate::Module as TwoPhase;

pub use frame_benchmarking::{account, benchmarks, whitelist_account, whitelisted_caller};
use frame_support::{assert_ok, traits::OnInitialize};
use frame_system::RawOrigin;
use rand::{prelude::SliceRandom, rngs::SmallRng, SeedableRng};
use sp_election_providers::Assignment;
use sp_arithmetic::traits::One;
use sp_runtime::InnerOf;
use sp_std::convert::TryInto;

const SEED: u32 = 0;

/// Creates a **valid** solution with exactly the given size.
///
/// The snapshot is also created internally.
fn solution_with_size<T: Config>(
	size: SolutionSize,
	active_voters_count: u32,
	desired_targets: u32,
) -> RawSolution<CompactOf<T>> {
	assert!(size.targets >= desired_targets, "must have enough targets");
	assert!(
		size.targets >= (<CompactOf<T>>::LIMIT * 2) as u32,
		"must have enough targets for unique votes."
	);
	assert!(size.voters >= active_voters_count, "must have enough voters");
	assert!(
		(<CompactOf<T>>::LIMIT as u32) < desired_targets,
		"must have enough winners to give them votes."
	);

	let ed: VoteWeight = T::Currency::minimum_balance().saturated_into::<u64>();
	let stake: VoteWeight = ed.max(One::one()).saturating_mul(100);

	// first generates random targets.
	let targets: Vec<T::AccountId> =
		(0..size.targets).map(|i| account("Targets", i, SEED)).collect();

	let mut rng = SmallRng::seed_from_u64(999u64);

	// decide who are the winners.
	let winners = targets
		.as_slice()
		.choose_multiple(&mut rng, desired_targets as usize)
		.cloned()
		.collect::<Vec<_>>();

	// first generate active voters who must vote for a subset of winners.
	let active_voters = (0..active_voters_count)
		.map(|i| {
			// chose a random subset of winners.
			let winner_votes = winners
				.as_slice()
				.choose_multiple(&mut rng, <CompactOf<T>>::LIMIT)
				.cloned()
				.collect::<Vec<_>>();
			let voter = account::<T::AccountId>("Voter", i, SEED);
			(voter, stake, winner_votes)
		})
		.collect::<Vec<_>>();

	// rest of the voters. They can only vote for non-winners.
	let non_winners =
		targets.iter().filter(|t| !winners.contains(t)).cloned().collect::<Vec<T::AccountId>>();
	let rest_voters = (active_voters_count..size.voters)
		.map(|i| {
			let votes = (&non_winners)
				.choose_multiple(&mut rng, <CompactOf<T>>::LIMIT)
				.cloned()
				.collect::<Vec<T::AccountId>>();
			let voter = account::<T::AccountId>("Voter", i, SEED);
			(voter, stake, votes)
		})
		.collect::<Vec<_>>();

	let mut all_voters = active_voters.clone();
	all_voters.extend(rest_voters);
	all_voters.shuffle(&mut rng);

	assert_eq!(active_voters.len() as u32, active_voters_count);
	assert_eq!(all_voters.len() as u32, size.voters);
	assert_eq!(winners.len() as u32, desired_targets);

	<SnapshotMetadata<T>>::put(RoundSnapshotMetadata {
		voters_len: all_voters.len() as u32,
		targets_len: targets.len() as u32,
	});
	<DesiredTargets<T>>::put(desired_targets);
	<Snapshot<T>>::put(RoundSnapshot { voters: all_voters.clone(), targets: targets.clone() });

	// write the snapshot to staking or whoever is the data provider.
	T::DataProvider::put_snapshot(all_voters.clone(), targets.clone());

	let cache = helpers::generate_voter_cache::<T>(&all_voters);
	let stake_of = helpers::stake_of_fn::<T>(&all_voters, &cache);
	let voter_index = helpers::voter_index_fn::<T>(&cache);
	let target_index = helpers::target_index_fn_linear::<T>(&targets);
	let voter_at = helpers::voter_at_fn::<T>(&all_voters);
	let target_at = helpers::target_at_fn::<T>(&targets);

	let assignments = active_voters
		.iter()
		.map(|(voter, _stake, votes)| {
			let percent_per_edge: InnerOf<CompactAccuracyOf<T>> =
				(100 / votes.len()).try_into().unwrap_or_else(|_| panic!("failed to convert"));
			Assignment {
				who: voter.clone(),
				distribution: votes
					.iter()
					.map(|t| (t.clone(), <CompactAccuracyOf<T>>::from_percent(percent_per_edge)))
					.collect::<Vec<_>>(),
			}
		})
		.collect::<Vec<_>>();

	let compact =
		<CompactOf<T>>::from_assignment(assignments, &voter_index, &target_index).unwrap();
	let score = compact.clone().score(&winners, stake_of, voter_at, target_at).unwrap();
	let round = <TwoPhase<T>>::round();
	RawSolution { compact, score, round }
}

benchmarks! {
	on_initialize_nothing {
		assert!(<TwoPhase<T>>::current_phase().is_off());
	}: {
		<TwoPhase<T>>::on_initialize(1u32.into());
	} verify {
		assert!(<TwoPhase<T>>::current_phase().is_off());
	}

	on_initialize_open_signed {
		// NOTE: this benchmark currently doesn't have any components because the length of a db
		// read/write is not captured. Otherwise, it is quite influenced by how much data
		// `T::ElectionDataProvider` is reading and passing on.
		assert!(<TwoPhase<T>>::snapshot().is_none());
		assert!(<TwoPhase<T>>::current_phase().is_off());
	}: {
		<TwoPhase<T>>::on_initialize_open_signed();
	} verify {
		assert!(<TwoPhase<T>>::snapshot().is_some());
		assert!(<TwoPhase<T>>::current_phase().is_signed());
	}

	on_initialize_open_unsigned_with_snapshot {
		assert!(<TwoPhase<T>>::snapshot().is_none());
		assert!(<TwoPhase<T>>::current_phase().is_off());
	}: {
		<TwoPhase<T>>::on_initialize_open_unsigned(true, true, 1u32.into());
	} verify {
		assert!(<TwoPhase<T>>::snapshot().is_some());
		assert!(<TwoPhase<T>>::current_phase().is_unsigned());
	}

	on_initialize_open_unsigned_without_snapshot {
		// need to assume signed phase was open before
		<TwoPhase<T>>::on_initialize_open_signed();
		assert!(<TwoPhase<T>>::snapshot().is_some());
		assert!(<TwoPhase<T>>::current_phase().is_signed());
	}: {
		<TwoPhase<T>>::on_initialize_open_unsigned(false, true, 1u32.into());
	} verify {
		assert!(<TwoPhase<T>>::snapshot().is_some());
		assert!(<TwoPhase<T>>::current_phase().is_unsigned());
	}

	#[extra]
	create_snapshot {
		assert!(<TwoPhase<T>>::snapshot().is_none());
	}: {
		<TwoPhase::<T>>::create_snapshot()
	} verify {
		assert!(<TwoPhase<T>>::snapshot().is_some());
	}

	submit_unsigned {
		// number of votes in snapshot.
		let v in (T::BenchmarkingConfig::VOTERS[0]) .. T::BenchmarkingConfig::VOTERS[1];
		// number of targets in snapshot.
		let t in (T::BenchmarkingConfig::TARGETS[0]) .. T::BenchmarkingConfig::TARGETS[1];
		// number of assignments, i.e. compact.len(). This means the active nominators, thus must be
		// a subset of `v` component.
		let a in (T::BenchmarkingConfig::ACTIVE_VOTERS[0]) .. T::BenchmarkingConfig::ACTIVE_VOTERS[1];
		// number of desired targets. Must be a subset of `t` component.
		let d in (T::BenchmarkingConfig::DESIRED_TARGETS[0]) .. T::BenchmarkingConfig::DESIRED_TARGETS[1];

		let witness = SolutionSize { voters: v, targets: t };
		let raw_solution = solution_with_size::<T>(witness, a, d);

		assert!(<TwoPhase<T>>::queued_solution().is_none());
		<CurrentPhase<T>>::put(Phase::Unsigned((true, 1u32.into())));
	}: _(RawOrigin::None, raw_solution, witness)
	verify {
		assert!(<TwoPhase<T>>::queued_solution().is_some());
	}

	// This is checking a valid solution. The worse case is indeed a valid solution.
	feasibility_check {
		// number of votes in snapshot.
		let v in (T::BenchmarkingConfig::VOTERS[0]) .. T::BenchmarkingConfig::VOTERS[1];
		// number of targets in snapshot.
		let t in (T::BenchmarkingConfig::TARGETS[0]) .. T::BenchmarkingConfig::TARGETS[1];
		// number of assignments, i.e. compact.len(). This means the active nominators, thus must be
		// a subset of `v` component.
		let a in (T::BenchmarkingConfig::ACTIVE_VOTERS[0]) .. T::BenchmarkingConfig::ACTIVE_VOTERS[1];
		// number of desired targets. Must be a subset of `t` component.
		let d in (T::BenchmarkingConfig::DESIRED_TARGETS[0]) .. T::BenchmarkingConfig::DESIRED_TARGETS[1];

		let size = SolutionSize { voters: v, targets: t };
		let raw_solution = solution_with_size::<T>(size, a, d);

		assert_eq!(raw_solution.compact.voter_count() as u32, a);
		assert_eq!(raw_solution.compact.unique_targets().len() as u32, d);
	}: {
		assert_ok!(<TwoPhase<T>>::feasibility_check(raw_solution, ElectionCompute::Unsigned));
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::mock::*;

	#[test]
	fn test_benchmarks() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_feasibility_check::<Runtime>());
		});

		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_submit_unsigned::<Runtime>());
		});

		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_on_initialize_open_unsigned_with_snapshot::<Runtime>());
		});

		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_on_initialize_open_unsigned_without_snapshot::<Runtime>());
		});

		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_on_initialize_nothing::<Runtime>());
		});

		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(test_benchmark_create_snapshot::<Runtime>());
		});
	}
}