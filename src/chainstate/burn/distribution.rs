/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::collections::BTreeMap;

use chainstate::burn::operations::BlockstackOperationType;
use chainstate::burn::operations::leader_key_register::LeaderKeyRegisterOp;
use chainstate::burn::operations::leader_block_commit::LeaderBlockCommitOp;
use chainstate::burn::operations::user_burn_support::UserBurnSupportOp;

use burnchains::Address;
use burnchains::PublicKey;
use burnchains::Burnchain;

use util::hash::Hash160;
use util::uint::Uint256;
use util::uint::Uint512;
use util::uint::BitArray;
use util::vrf::ECVRF_public_key_to_hex;

use util::log;

#[derive(Debug, Clone, PartialEq)]
pub struct BurnSamplePoint<A, K>
where
    A: Address,
    K: PublicKey
{
    pub burns: u128,
    pub range_start: Uint256,
    pub range_end: Uint256,
    pub candidate: LeaderBlockCommitOp<A, K>,
    pub key: LeaderKeyRegisterOp<A, K>,
    pub user_burns: Vec<UserBurnSupportOp<A, K>>
}

impl<A, K> BurnSamplePoint<A, K>
where
    A: Address,
    K: PublicKey
{
   
    /// Make a burn distribution -- a list of (burn total, block candidate) pairs -- from a block's
    /// block commits, leader keys, and user support burns.
    ///
    /// All operations need to be from the same block height, or this method panics.
    ///
    /// Returns the distribution, which consumes the given lists of operations.
    pub fn make_distribution(block_candidates: Vec<LeaderBlockCommitOp<A, K>>, consumed_leader_keys: Vec<LeaderKeyRegisterOp<A,K>>, user_burns: Vec<UserBurnSupportOp<A, K>>) -> Vec<BurnSamplePoint<A, K>> {
        // trivial case
        if block_candidates.len() == 0 {
            return vec![];
        }

        BurnSamplePoint::ops_sanity_checks(&block_candidates, &consumed_leader_keys, &user_burns);

        // map each leader key's position in the blockchain to its index in consumed_leader_keys.
        // The DB will ensure that there are no duplicates.
        let mut key_index : BTreeMap<(u64, u32), usize> = BTreeMap::new();
        for i in 0..consumed_leader_keys.len() {
            let key_loc = (consumed_leader_keys[i].block_number, consumed_leader_keys[i].vtxindex);
            assert!(key_index.get(&key_loc).is_none());

            key_index.insert(key_loc, i);
        }

        // consume block candidates and leader keys to produce the list of burn sample points
        let mut burn_sample : Vec<BurnSamplePoint<A, K>> = block_candidates
            .into_iter()
            .map(|bc| {
                let key_idx_opt = key_index.get(&(bc.block_number - (bc.key_block_backptr as u64), bc.key_vtxindex as u32));
                if key_idx_opt.is_none() {
                    // should never happen -- the DB only accepts a leader block commitment if it
                    // matches a corresponding VRF public key 
                    panic!("No leader key for block commitment {} (at {},{}) -- points to ({},{})", 
                           &bc.txid.to_hex(), bc.block_number, bc.vtxindex, bc.block_number - (bc.key_block_backptr as u64), bc.key_vtxindex);
                }
                let key_idx = key_idx_opt.unwrap();

                BurnSamplePoint {
                    burns: bc.burn_fee as u128,     // Initial burn weight is the block commitment's burn
                    range_start: Uint256::zero(),   // To be filled in
                    range_end: Uint256::zero(),     // To be filled in
                    candidate: bc,
                    key: consumed_leader_keys[*key_idx].clone(),
                    user_burns: vec![]
                }
            })
            .collect();

        // assign user burns to the burn sample points 
        BurnSamplePoint::apply_user_burns(&mut burn_sample, user_burns);

        // calculate burn ranges 
        BurnSamplePoint::make_sortition_ranges(&mut burn_sample);
        burn_sample
    }
    
    // sanity checks for making a burn distribution
    fn ops_sanity_checks(block_candidates: &Vec<LeaderBlockCommitOp<A, K>>, consumed_leader_keys: &Vec<LeaderKeyRegisterOp<A,K>>, user_burns: &Vec<UserBurnSupportOp<A, K>>) -> () {
        // sanity checks
        if block_candidates.len() != consumed_leader_keys.len() {
            panic!("FATAL ERROR: {} block candidates != {} leader keys", block_candidates.len(), consumed_leader_keys.len());
        }

        let block_height = block_candidates[0].block_number;
        for i in 1..block_candidates.len() {
            if block_candidates[i].block_number != block_height {
                panic!("FATAL ERROR: block commit {} is at ({},{}) not {}", &block_candidates[i].txid.to_hex(), block_candidates[i].block_number, block_candidates[i].vtxindex, block_height);
            }
        }

        for i in 0..user_burns.len() {
            if user_burns[i].block_number != block_height {
                panic!("FATAL ERROR: user burn {} is at ({},{}) not {}", &user_burns[i].txid.to_hex(), user_burns[i].block_number, user_burns[i].vtxindex, block_height);
            }
        }

        return;
    }

    /// Move a list of user burns into their burn sample points.
    fn apply_user_burns(burn_sample: &mut Vec<BurnSamplePoint<A, K>>, user_burns: Vec<UserBurnSupportOp<A, K>>) -> () {
        // map each burn sample's (VRF public key, block hash 160) pair to its index.
        // The pair will be unique, since the DB requires each VRF key to be unique.
        // Hovever, the block hash 160 is not guaranteed to be unique -- two different
        // leaders can burn for the same block.
        let mut burn_index : BTreeMap<(String, Hash160), usize> = BTreeMap::new();
        for i in 0..burn_sample.len() {
            let block_header_hash_160 = Hash160::from_sha256(&burn_sample[i].candidate.block_header_hash.as_bytes());
            let vrf_hex = ECVRF_public_key_to_hex(&burn_sample[i].key.public_key);
            assert!(burn_index.get(&(vrf_hex.clone(), block_header_hash_160)).is_none());

            burn_index.insert((vrf_hex, block_header_hash_160), i);
        }

        // match user burns to the leaders
        for user_burn in user_burns {
            let block_idx_opt = burn_index.get(&(ECVRF_public_key_to_hex(&user_burn.public_key), user_burn.block_header_hash_160));
            match block_idx_opt {
                Some(ref_block_idx) => {
                    // user burn matches a (key, block) pair committed to in this block
                    let idx = *ref_block_idx;
                    burn_sample[idx].burns += user_burn.burn_fee as u128;
                    burn_sample[idx].user_burns.push(user_burn);
                },
                None => {
                    info!("User burn {} ({},{}) of {} for key={}, block={} has no matching block commit",
                          &user_burn.txid.to_hex(), user_burn.block_number, user_burn.vtxindex, user_burn.burn_fee,
                          ECVRF_public_key_to_hex(&user_burn.public_key), &user_burn.block_header_hash_160.to_hex());
                    continue;
                }
            };
        }
    }

    /// Calculate the ranges between 0 and 2**256 - 1 over which each point in the burn sample
    /// applies, so we can later select which block to use.
    fn make_sortition_ranges(burn_sample: &mut Vec<BurnSamplePoint<A, K>>) -> () {
        if burn_sample.len() == 0 {
            // empty sample
            return;
        }
        if burn_sample.len() == 1 {
            // sample that covers the whole range 
            burn_sample[0].range_start = Uint256::zero();
            burn_sample[0].range_end = Uint256::max();
            return;
        }

        // total burns for valid blocks?
        // NOTE: this can't overflow -- there's no way we get that many (u64) burns
        let total_burns_u128 = BurnSamplePoint::get_total_burns(&burn_sample).unwrap() as u128;
        let total_burns = Uint512::from_u128(total_burns_u128);

        // determine range start/end for each sample.
        // Use fixed-point math on an unsigned 512-bit number -- 
        //   * the upper 256 bits are the integer
        //   * the lower 256 bits are the fraction
        // These range fields correspond to ranges in the 32-byte hash space
        let mut burn_acc = Uint512::from_u128(burn_sample[0].burns);

        burn_sample[0].range_start = Uint256::zero();
        burn_sample[0].range_end = ((Uint512::from_uint256(&Uint256::max()) * burn_acc) / total_burns).to_uint256();
        for i in 1..burn_sample.len() {
            burn_sample[i].range_start = burn_sample[i-1].range_end;
            
            burn_acc = burn_acc + Uint512::from_u128(burn_sample[i].burns);
            burn_sample[i].range_end = ((Uint512::from_uint256(&Uint256::max()) * burn_acc) / total_burns).to_uint256();
        }

        for i in 0..burn_sample.len() {
            test_debug!("Range for block {}: {} / {}: {} - {}", burn_sample[i].candidate.block_header_hash.to_hex(), burn_sample[i].burns, total_burns_u128, burn_sample[i].range_start, burn_sample[i].range_end);
        }
    }

    /// Calculate the total amount of crypto destroyed in this burn distribution.
    /// Returns None if there was an overflow.
    pub fn get_total_burns(burn_dist: &Vec<BurnSamplePoint<A, K>>) -> Option<u64> {
        let block_burn_total_u128 : u128 = burn_dist
            .iter()
            .fold(0u128, |mut burns_so_far, sample_point| {
                burns_so_far += sample_point.burns;
                burns_so_far
            });

        // check overflow
        if block_burn_total_u128 >= 0xffffffffffffffff {
            error!("Excessive burn size {}", block_burn_total_u128);
            return None;
        }
        let block_burn_total = block_burn_total_u128 as u64;
        Some(block_burn_total)
    }
}

#[cfg(test)]
mod tests {
    use super::BurnSamplePoint;

    use std::marker::PhantomData;

    use burnchains::Address;
    use burnchains::PublicKey;
    use burnchains::Burnchain;
    use burnchains::BurnchainTxInput;
    use burnchains::BurnchainInputType;

    use chainstate::burn::operations::leader_key_register::LeaderKeyRegisterOp;
    use chainstate::burn::operations::leader_block_commit::LeaderBlockCommitOp;
    use chainstate::burn::operations::user_burn_support::UserBurnSupportOp;
    use chainstate::burn::operations::leader_block_commit::OPCODE as LeaderBlockCommitOpcode;
    use chainstate::burn::operations::leader_key_register::OPCODE as LeaderKeyRegisterOpcode;
    use chainstate::burn::operations::user_burn_support::OPCODE as UserBurnSupportOpcode;

    use burnchains::bitcoin::address::BitcoinAddress;
    use burnchains::bitcoin::keys::BitcoinPublicKey;
    use burnchains::bitcoin::BitcoinNetworkType;
    
    use burnchains::{Txid, BurnchainHeaderHash};
    use chainstate::burn::{ConsensusHash, VRFSeed, BlockHeaderHash};
    use util::hash::hex_bytes;
    use ed25519_dalek::PublicKey as VRFPublicKey;

    use util::hash::Hash160;
    use util::uint::Uint256;
    use util::uint::Uint512;
    use util::uint::BitArray;
    use util::vrf::ECVRF_public_key_to_hex;

    use util::log;

    struct BurnDistFixture {
        consumed_leader_keys: Vec<LeaderKeyRegisterOp<BitcoinAddress, BitcoinPublicKey>>,
        block_commits: Vec<LeaderBlockCommitOp<BitcoinAddress, BitcoinPublicKey>>,
        user_burns: Vec<UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey>>,
        res: Vec<BurnSamplePoint<BitcoinAddress, BitcoinPublicKey>>
    }

    #[test]
    fn make_burn_distribution() {

        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let leader_key_1 : LeaderKeyRegisterOp<BitcoinAddress, BitcoinPublicKey> = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap(),

            op: LeaderKeyRegisterOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
            vtxindex: 456,
            block_number: 123,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000001").unwrap(),
            
            _phantom: PhantomData
        };
        
        let leader_key_2 : LeaderKeyRegisterOp<BitcoinAddress, BitcoinPublicKey> = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("3333333333333333333333333333333333333333").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a91432b6c66189da32bd0a9f00ee4927f569957d71aa88ac").unwrap()).unwrap(),

            op: LeaderKeyRegisterOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("9410df84e2b440055c33acb075a0687752df63fe8fe84aeec61abe469f0448c7").unwrap()).unwrap(),
            vtxindex: 457,
            block_number: 122,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
            
            _phantom: PhantomData
        };

        let leader_key_3 : LeaderKeyRegisterOp<BitcoinAddress, BitcoinPublicKey> = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("3333333333333333333333333333333333333333").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("de8af7037e522e65d2fe2d63fb1b764bfea829df78b84444338379df13144a02").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a91432b6c66189da32bd0a9f00ee4927f569957d71aa88ac").unwrap()).unwrap(),

            op: LeaderKeyRegisterOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("eb54704f71d4a2d1128d60ffccced547054b52250ada6f3e7356165714f44d4c").unwrap()).unwrap(),
            vtxindex: 10,
            block_number: 121,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000012").unwrap(),
            
            _phantom: PhantomData
        };

        let user_burn_noblock : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("3333333333333333333333333333333333333333").unwrap()).unwrap(),
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 12345,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716c").unwrap()).unwrap(),
            vtxindex: 12,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };

        let user_burn_1 : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("7150f635054b87df566a970b21e07030d6444bf2").unwrap()).unwrap(),       // 22222....2222
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 10000,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716c").unwrap()).unwrap(),
            vtxindex: 13,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };
        
        let user_burn_1_2 : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("7150f635054b87df566a970b21e07030d6444bf2").unwrap()).unwrap(),       // 22222....2222
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 30000,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716c").unwrap()).unwrap(),
            vtxindex: 14,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };

        let user_burn_2 : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),       // 22222....2223
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 20000,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716d").unwrap()).unwrap(),
            vtxindex: 14,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };
        
        let user_burn_2_2 : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),       // 22222....2223
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 40000,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716c").unwrap()).unwrap(),
            vtxindex: 15,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };

        let user_burn_nokey : UserBurnSupportOp<BitcoinAddress, BitcoinPublicKey> = UserBurnSupportOp {
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("4444444444444444444444444444444444444444").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("3f3338db51f2b1f6ac0cf6177179a24ee130c04ef2f9849a64a216969ab60e70").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),
            memo: vec![0x01, 0x02, 0x03, 0x04, 0x05],
            burn_fee: 12345,

            op: UserBurnSupportOpcode,
            txid: Txid::from_bytes_be(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716e").unwrap()).unwrap(),
            vtxindex: 15,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),
            
            _phantom_a: PhantomData,
            _phantom_k: PhantomData
        };

        let block_commit_1 : LeaderBlockCommitOp<BitcoinAddress, BitcoinPublicKey> = LeaderBlockCommitOp {
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
            parent_block_backptr: 123,
            parent_vtxindex: 456,
            key_block_backptr: 1,
            key_vtxindex: 456,
            epoch_num: 50,
            memo: vec![0x80],

            burn_fee: 12345,
            input: BurnchainTxInput {
                keys: vec![
                    BitcoinPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_required: 1, 
                in_type: BurnchainInputType::BitcoinInput,
            },

            op: 91,     // '[' in ascii
            txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
            vtxindex: 444,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),

            _phantom: PhantomData
        };        
        
        let block_commit_2 : LeaderBlockCommitOp<BitcoinAddress, BitcoinPublicKey> = LeaderBlockCommitOp {
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222223").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333334").unwrap()).unwrap(),
            parent_block_backptr: 123,
            parent_vtxindex: 111,
            key_block_backptr: 2,
            key_vtxindex: 457,
            epoch_num: 50,
            memo: vec![0x80],

            burn_fee: 12345,
            input: BurnchainTxInput {
                keys: vec![
                    BitcoinPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_required: 1, 
                in_type: BurnchainInputType::BitcoinInput,
            },

            op: 91,     // '[' in ascii
            txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27d0").unwrap()).unwrap(),
            vtxindex: 445,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),

            _phantom: PhantomData
        };        
        
        let block_commit_3 : LeaderBlockCommitOp<BitcoinAddress, BitcoinPublicKey> = LeaderBlockCommitOp {
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222224").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333335").unwrap()).unwrap(),
            parent_block_backptr: 123,
            parent_vtxindex: 111,
            key_block_backptr: 3,
            key_vtxindex: 10,
            epoch_num: 50,
            memo: vec![0x80],

            burn_fee: 23456,
            input: BurnchainTxInput {
                keys: vec![
                    BitcoinPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_required: 1, 
                in_type: BurnchainInputType::BitcoinInput,
            },

            op: 91,     // '[' in ascii
            txid: Txid::from_bytes_be(&hex_bytes("301dc687a9f06a1ae87a013f27133e9cec0843c2983567be73e185827c7c13de").unwrap()).unwrap(),
            vtxindex: 445,
            block_number: 124,
            burn_header_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap(),

            _phantom: PhantomData
        };

        /*
         You can generate the burn sample ranges with this Python script:
         #!/usr/bin/python

         import sys

         a = eval(sys.argv[1])
         b = eval(sys.argv[2])

         s = '{:0128x}'.format((a * (2**256 - 1)) / b).decode('hex')[::-1];
         l = ['0x{:016x}'.format(int(s[(8*i):(8*(i+1))][::-1].encode('hex'),16)) for i in range(0,(256/8/8))]

         print float(a) / b
         print '{:0128x}'.format((a * (2**256 - 1)) / b)
         print '[' + ', '.join(l) + ']'
        */

        let fixtures : Vec<BurnDistFixture> = vec![
            BurnDistFixture {
                consumed_leader_keys: vec![],
                block_commits: vec![],
                user_burns: vec![],
                res: vec![],
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                ],
                user_burns: vec![],
                res: vec![
                    BurnSamplePoint {
                        burns: block_commit_1.burn_fee.into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256::max(),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![]
                    }
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![],
                res: vec![
                    BurnSamplePoint {
                        burns: block_commit_1.burn_fee.into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![],
                    },
                    BurnSamplePoint {
                        burns: block_commit_2.burn_fee.into(),
                        range_start: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![
                    user_burn_noblock.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: block_commit_1.burn_fee.into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![],
                    },
                    BurnSamplePoint {
                        burns: block_commit_2.burn_fee.into(),
                        range_start: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![
                    user_burn_nokey.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: block_commit_1.burn_fee.into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![],
                    },
                    BurnSamplePoint {
                        burns: block_commit_2.burn_fee.into(),
                        range_start: Uint256([0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0x7fffffffffffffff]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![
                    user_burn_nokey.clone(),
                    user_burn_noblock.clone(),
                    user_burn_1.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: (block_commit_1.burn_fee + user_burn_1.burn_fee).into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0x441d393138e5a796, 0xbada4a3d4046d839, 0xa24749933957018c, 0xa4e5f328cf38744d]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![
                            user_burn_1.clone(),
                        ],
                    },
                    BurnSamplePoint {
                        burns: block_commit_2.burn_fee.into(),
                        range_start: Uint256([0x441d393138e5a796, 0xbada4a3d4046d839, 0xa24749933957018c, 0xa4e5f328cf38744d]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![
                    user_burn_nokey.clone(),
                    user_burn_noblock.clone(),
                    user_burn_1.clone(),
                    user_burn_2.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: (block_commit_1.burn_fee + user_burn_1.burn_fee).into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0x65db6527a5c06ed7, 0xfbf9725ae754dd80, 0xeafb8d991cf9964d, 0x6898693a2f1713b4]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![
                            user_burn_1.clone(),
                        ],
                    },
                    BurnSamplePoint {
                        burns: (block_commit_2.burn_fee + user_burn_2.burn_fee).into(),
                        range_start: Uint256([0x65db6527a5c06ed7, 0xfbf9725ae754dd80, 0xeafb8d991cf9964d, 0x6898693a2f1713b4]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![
                            user_burn_2.clone()
                        ],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                ],
                user_burns: vec![
                    user_burn_nokey.clone(),
                    user_burn_noblock.clone(),
                    user_burn_2_2.clone(),
                    user_burn_1_2.clone(),
                    user_burn_1.clone(),
                    user_burn_2.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: (block_commit_1.burn_fee + user_burn_1.burn_fee + user_burn_1_2.burn_fee).into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0xbc9e168afe8ad47e, 0xbbb6d3eb8d1be6c9, 0x45a410039d0a7dc5, 0x6b7815d84b0f9fc0]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![
                            user_burn_1_2.clone(),
                            user_burn_1.clone(),
                        ],
                    },
                    BurnSamplePoint {
                        burns: (block_commit_2.burn_fee + user_burn_2.burn_fee + user_burn_2_2.burn_fee).into(),
                        range_start: Uint256([0xbc9e168afe8ad47e, 0xbbb6d3eb8d1be6c9, 0x45a410039d0a7dc5, 0x6b7815d84b0f9fc0]),
                        range_end: Uint256::max(),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![
                            user_burn_2_2.clone(),
                            user_burn_2.clone()
                        ],
                    },
                ]
            },
            BurnDistFixture {
                consumed_leader_keys: vec![
                    leader_key_1.clone(),
                    leader_key_2.clone(),
                    leader_key_3.clone(),
                ],
                block_commits: vec![
                    block_commit_1.clone(),
                    block_commit_2.clone(),
                    block_commit_3.clone(),
                ],
                user_burns: vec![
                    user_burn_nokey.clone(),
                    user_burn_noblock.clone(),
                    user_burn_2_2.clone(),
                    user_burn_1_2.clone(),
                    user_burn_1.clone(),
                    user_burn_2.clone(),
                ],
                res: vec![
                    BurnSamplePoint {
                        burns: (block_commit_1.burn_fee + user_burn_1.burn_fee + user_burn_1_2.burn_fee).into(),
                        range_start: Uint256::zero(),
                        range_end: Uint256([0xcb48ed15c5086a5c, 0x6b29682cfbe4089c, 0x4a30e732285c18c9, 0x5a7416b691bddbad]),
                        candidate: block_commit_1.clone(),
                        key: leader_key_1.clone(),
                        user_burns: vec![
                            user_burn_1_2.clone(),
                            user_burn_1.clone(),
                        ],
                    },
                    BurnSamplePoint {
                        burns: (block_commit_2.burn_fee + user_burn_2.burn_fee + user_burn_2_2.burn_fee).into(),
                        range_start: Uint256([0xcb48ed15c5086a5c, 0x6b29682cfbe4089c, 0x4a30e732285c18c9, 0x5a7416b691bddbad]),
                        range_end: Uint256([0xa224e0451efa00f5, 0xa57394a7b38d5b1c, 0x6bfdbf24cdb0b617, 0xd777aa6d9e769e59]),
                        candidate: block_commit_2.clone(),
                        key: leader_key_2.clone(),
                        user_burns: vec![
                            user_burn_2_2.clone(),
                            user_burn_2.clone()
                        ],
                    },
                    BurnSamplePoint {
                        burns: (block_commit_3.burn_fee).into(),
                        range_start: Uint256([0xa224e0451efa00f5, 0xa57394a7b38d5b1c, 0x6bfdbf24cdb0b617, 0xd777aa6d9e769e59]),
                        range_end: Uint256::max(),
                        candidate: block_commit_3.clone(),
                        key: leader_key_3.clone(),
                        user_burns: vec![]
                    },
                ]
            },
        ];

        for i in 0..fixtures.len() {
            let f = &fixtures[i];
            let dist = BurnSamplePoint::make_distribution(f.block_commits.iter().cloned().collect(), f.consumed_leader_keys.iter().cloned().collect(), f.user_burns.iter().cloned().collect());
            assert_eq!(dist, f.res);
        }
    }
}