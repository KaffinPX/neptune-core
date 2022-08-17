use anyhow::Result;
use memmap2::MmapOptions;
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    ops::DerefMut,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::Mutex as TokioMutex;
use tracing::debug;
use twenty_first::amount::u32s::U32s;

use super::shared::{get_block_file_path, new_block_file_is_needed};
use crate::{
    database::leveldb::LevelDB,
    models::{
        blockchain::{
            block::{block_header::PROOF_OF_WORK_COUNT_U32_SIZE, Block},
            digest::Digest,
        },
        database::{
            BlockDatabases, BlockFileLocation, BlockIndexKey, BlockIndexValue, BlockRecord,
            FileRecord, LastFileRecord,
        },
    },
};

#[derive(Clone, Debug)]
pub struct ArchivalState {
    // Since this is a database, we use the tokio Mutex here.
    pub block_databases: Arc<TokioMutex<BlockDatabases>>,

    root_data_dir: PathBuf,

    // The genesis block is stored on the heap, as we would otherwise get stack overflows whenever we instantiate
    // this object in a spawned worker thread.
    genesis_block: Box<Block>,
}

impl ArchivalState {
    pub fn new(
        initial_block_databases: Arc<TokioMutex<BlockDatabases>>,
        root_data_dir: PathBuf,
    ) -> Self {
        Self {
            block_databases: initial_block_databases,
            root_data_dir,
            genesis_block: Box::new(Block::genesis_block()),
        }
    }

    /// Write a newly found block to database and to disk. A lock should be held over light state
    /// while this function call is executed.
    pub fn write_block(
        &self,
        new_block: Box<Block>,
        db_lock: &mut tokio::sync::MutexGuard<'_, BlockDatabases>,
        current_max_pow_family: Option<U32s<PROOF_OF_WORK_COUNT_U32_SIZE>>,
    ) -> Result<()> {
        // TODO: Multiple blocks can have the same height: fix!
        db_lock
            .block_height_to_hash
            .put(new_block.header.height, new_block.hash);
        db_lock
            .block_hash_to_block
            .put(new_block.hash, *new_block.clone());
        db_lock
            .latest_block_header
            .put((), new_block.header.clone());

        // Write block to disk
        let mut last_rec: LastFileRecord = match db_lock
            .block_index
            .get(BlockIndexKey::LastFile)
            .map(|x| x.as_last_file_record())
        {
            Some(rec) => rec,
            None => LastFileRecord::default(),
        };

        // This file must exist on disk already, unless this is the first block
        // stored on disk.
        let mut block_file_path =
            get_block_file_path(self.root_data_dir.clone(), last_rec.last_file);
        let serialized_block: Vec<u8> = bincode::serialize(&new_block).unwrap();
        let serialized_block_size: u64 = serialized_block.len() as u64;
        let mut block_file: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(block_file_path.clone())
            .unwrap();
        if new_block_file_is_needed(&block_file, serialized_block_size) {
            last_rec = LastFileRecord {
                last_file: last_rec.last_file + 1,
            };
            block_file_path = get_block_file_path(self.root_data_dir.clone(), last_rec.last_file);
            block_file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(block_file_path.clone())
                .unwrap();
        }

        debug!("Writing block to: {}", block_file_path.display());
        // Get associated file record from database, otherwise create it
        let file_record_key: BlockIndexKey = BlockIndexKey::File(last_rec.last_file);
        let file_record_value: Option<FileRecord> = db_lock
            .block_index
            .get(file_record_key.clone())
            .map(|x| x.as_file_record());
        let file_record_value: FileRecord = match file_record_value {
            Some(record) => record.add(serialized_block_size, &new_block.header),
            None => FileRecord::new(serialized_block_size, &new_block.header),
        };

        // Make room in file for mmapping and record where block starts
        let pos = block_file.seek(SeekFrom::End(0)).unwrap();
        debug!("Opened file offset: {}", pos);
        block_file
            .seek(SeekFrom::Current(serialized_block_size as i64 - 1))
            .unwrap();
        block_file.write_all(&[0]).unwrap();
        let file_offset: u64 = block_file
            .seek(SeekFrom::Current(-(serialized_block_size as i64)))
            .unwrap();
        debug!(
            "New file size: {} bytes",
            block_file.metadata().unwrap().len()
        );

        let height_record_key = BlockIndexKey::Height(new_block.header.height);
        let mut blocks_at_same_height: Vec<Digest> =
            match db_lock.block_index.get(height_record_key.clone()) {
                Some(rec) => rec.as_height_record(),
                None => vec![],
            };

        // Write to file with mmap, only map relevant part of file into memory
        let mmap = unsafe {
            MmapOptions::new()
                .offset(pos)
                .len(serialized_block_size as usize)
                .map(&block_file)?
        };
        let mut mmap: memmap2::MmapMut = mmap.make_mut().unwrap();
        mmap.deref_mut()[..].copy_from_slice(&serialized_block);

        // Update block index database with newly stored block
        let mut block_index_entries: Vec<(BlockIndexKey, BlockIndexValue)> = vec![];
        let block_record_key: BlockIndexKey = BlockIndexKey::Block(new_block.hash);
        let block_record_value: BlockIndexValue = BlockIndexValue::Block(Box::new(BlockRecord {
            block_header: new_block.header.clone(),
            file_location: BlockFileLocation {
                file_index: last_rec.last_file,
                offset: file_offset,
                block_length: serialized_block_size as usize,
            },
            tx_count: new_block.body.transactions.len() as u32,
        }));
        debug!("New block record: {:?}", block_record_value);

        block_index_entries.push((file_record_key, BlockIndexValue::File(file_record_value)));
        block_index_entries.push((block_record_key, block_record_value));

        // Missing: height record and last record
        block_index_entries.push((BlockIndexKey::LastFile, BlockIndexValue::LastFile(last_rec)));
        blocks_at_same_height.push(new_block.hash);
        block_index_entries.push((
            height_record_key,
            BlockIndexValue::Height(blocks_at_same_height),
        ));

        // Mark block as tip if its PoW family is larger than current most canonical
        if current_max_pow_family.is_none()
            || current_max_pow_family.unwrap() < new_block.header.proof_of_work_family
        {
            block_index_entries.push((
                BlockIndexKey::BlockTipDigest,
                BlockIndexValue::BlockTipDigest(new_block.hash),
            ));
        }

        db_lock.block_index.batch_write(&block_index_entries);

        Ok(())
    }

    fn get_block_from_block_record(&self, block_record: BlockRecord) -> Result<Block> {
        // Get path of file for block
        let block_file_path: PathBuf = get_block_file_path(
            self.root_data_dir.clone(),
            block_record.file_location.file_index,
        );

        // Open file as read-only
        let block_file: fs::File = fs::OpenOptions::new()
            .read(true)
            .open(block_file_path)
            .unwrap();

        // Read the file into memory, set the offset and length indicated in the block record
        // to avoid using more memory than needed
        let mmap = unsafe {
            MmapOptions::new()
                .offset(block_record.file_location.offset)
                .len(block_record.file_location.block_length)
                .map(&block_file)?
        };
        let block: Block = bincode::deserialize(&mmap).unwrap();

        Ok(block)
    }

    /// Given a mutex lock on the database, return the latest block
    fn get_latest_block_from_disk(
        &self,
        databases: &mut tokio::sync::MutexGuard<BlockDatabases>,
    ) -> Result<Option<Block>> {
        let tip_digest = databases.block_index.get(BlockIndexKey::BlockTipDigest);
        let tip_digest: Digest = match tip_digest {
            Some(digest) => digest.as_tip_digest(),
            None => return Ok(None),
        };

        let tip_block_record: BlockRecord = databases
            .block_index
            .get(BlockIndexKey::Block(tip_digest))
            .unwrap()
            .as_block_record();

        let block: Block = self.get_block_from_block_record(tip_block_record)?;

        Ok(Some(block))
    }

    /// Return latest block from database, or genesis block if no other block
    /// is known.
    pub async fn get_latest_block(&self) -> Block {
        let mut dbs = self.block_databases.lock().await;
        let lookup_res_info: Option<Block> = self
            .get_latest_block_from_disk(&mut dbs)
            .expect("Failed to read block from disk");

        match lookup_res_info {
            None => *self.genesis_block.clone(),
            Some(block) => block,
        }
    }

    // Return the block with a given block digest, iff it's available in state somewhere
    pub async fn get_block(&self, block_digest: Digest) -> Result<Option<Block>> {
        let maybe_record: Option<BlockRecord> = self
            .block_databases
            .lock()
            .await
            .block_index
            .get(BlockIndexKey::Block(block_digest))
            .map(|x| x.as_block_record());
        let record: BlockRecord = match maybe_record {
            Some(rec) => rec,
            None => {
                if self.genesis_block.hash == block_digest {
                    return Ok(Some(*self.genesis_block.clone()));
                } else {
                    return Ok(None);
                }
            }
        };

        // Fetch block from disk
        let block = self.get_block_from_block_record(record)?;

        Ok(Some(block))
    }

    /// Return a list of digests of the ancestors to the requested digest. Does not include the input
    /// digest. If no ancestors can be found, returns the empty list. The count is the maximum length
    /// of the returned list. E.g. if the input digest corresponds to height 2 and count is 5, the
    /// returned list will contain the digests of block 1 and block 0 (the genesis block).
    /// The input block must correspond to a known block but it can be the genesis block in which case
    /// the empty list will be returned. The lock on the database must be free for this method to
    /// not end in a deadlock.
    pub async fn get_ancestor_block_digests(
        &self,
        block_digest: Digest,
        mut count: usize,
    ) -> Vec<Digest> {
        // TODO: This can be rewritten to only fetch block headers
        let input_block = self
            .get_block(block_digest)
            .await
            .expect("block lookup must succeed")
            .unwrap();
        let mut parent_digest = input_block.header.prev_block_digest;
        let mut ret = vec![];
        while let Some(parent) = self
            .get_block(parent_digest)
            .await
            .expect("block lookup must succeed")
        {
            ret.push(parent.hash);
            parent_digest = parent.header.prev_block_digest;
            count -= 1;
            if count == 0 {
                break;
            }
        }

        ret
    }
}

#[cfg(test)]
mod archival_state_tests {
    use super::*;

    use rand::{thread_rng, RngCore};
    use tracing_test::traced_test;

    use crate::{
        config_models::network::Network,
        models::state::{blockchain_state::BlockchainState, light_state::LightState},
        tests::shared::{add_block_to_archival_state, databases, make_mock_block},
    };

    #[traced_test]
    #[tokio::test]
    async fn initialize_archival_state_test() -> Result<()> {
        // Ensure that the archival state can be initialized without overflowing the stack
        tokio::spawn(async move {
            let (block_databases, _, data_dir) = databases(Network::Main).unwrap();
            let _archival_state0 = ArchivalState::new(block_databases, data_dir.clone());
            let (block_databases, _, data_dir) = databases(Network::Main).unwrap();
            let _archival_state1 = ArchivalState::new(block_databases, data_dir.clone());
            let (block_databases, _, data_dir) = databases(Network::Main).unwrap();
            let _archival_state2 = ArchivalState::new(block_databases, data_dir);
            let b = Block::genesis_block();
            let blockchain_state = BlockchainState {
                archival_state: Some(_archival_state2),
                light_state: LightState::new(_archival_state1.genesis_block.header),
            };
            let block_1 = make_mock_block(b, None);
            let mut lock0 = blockchain_state
                .archival_state
                .as_ref()
                .unwrap()
                .block_databases
                .lock()
                .await;
            lock0.block_hash_to_block.put(block_1.hash, block_1.clone());
            let c = lock0.block_hash_to_block.get(block_1.hash).unwrap();
            println!("genesis digest = {}", c.hash);
            drop(lock0);

            let mut lock1 = blockchain_state
                .archival_state
                .as_ref()
                .unwrap()
                .block_databases
                .lock()
                .await;
            let c = lock1.block_hash_to_block.get(block_1.hash).unwrap();
            println!("genesis digest = {}", c.hash);
        })
        .await?;

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    async fn get_latest_block_test() -> Result<()> {
        let (block_databases, _, root_data_dir_path) = databases(Network::Main).unwrap();
        println!("root_data_dir_path = {:?}", root_data_dir_path);
        let archival_state = ArchivalState::new(block_databases.clone(), root_data_dir_path);
        let mut db_lock_0 = block_databases.lock().await;
        let ret = archival_state.get_latest_block_from_disk(&mut db_lock_0)?;
        assert!(
            ret.is_none(),
            "Must return None when no block is stored in DB"
        );
        drop(db_lock_0);

        // Add a block to archival state and verify that this is returned
        let genesis = *archival_state.genesis_block.clone();
        let mock_block_1 = make_mock_block(genesis.clone(), None);
        add_block_to_archival_state(&archival_state, mock_block_1.clone()).await?;
        let mut db_lock_1 = block_databases.lock().await;
        let ret1 = archival_state.get_latest_block_from_disk(&mut db_lock_1)?;
        assert!(
            ret1.is_some(),
            "Must return a block when one is stored to DB"
        );
        assert_eq!(
            mock_block_1,
            ret1.unwrap(),
            "Returned block must match the one inserted"
        );
        drop(db_lock_1);

        // Add a 2nd block and verify that this new block is now returned
        let mock_block_2 = make_mock_block(mock_block_1, None);
        add_block_to_archival_state(&archival_state, mock_block_2.clone()).await?;
        let mut db_lock_2 = block_databases.lock().await;
        let ret2 = archival_state.get_latest_block_from_disk(&mut db_lock_2)?;
        assert!(
            ret2.is_some(),
            "Must return a block when one is stored to DB"
        );

        assert_eq!(
            mock_block_2,
            ret2.unwrap(),
            "Returned block must match the one inserted"
        );

        Ok(())
    }

    #[traced_test]
    #[tokio::test]
    async fn get_block_test() -> Result<()> {
        let (block_databases, _, root_data_dir_path) = databases(Network::Main).unwrap();
        let archival_state = ArchivalState::new(block_databases.clone(), root_data_dir_path);
        let genesis = *archival_state.genesis_block.clone();
        let mock_block_1 = make_mock_block(genesis.clone(), None);

        // Lookup a block in an empty database, expect None to be returned
        let ret0 = archival_state.get_block(mock_block_1.hash).await?;
        assert!(
            ret0.is_none(),
            "Must return a block when one is stored to DB"
        );

        add_block_to_archival_state(&archival_state, mock_block_1.clone()).await?;
        let ret1 = archival_state.get_block(mock_block_1.hash).await?;
        assert!(
            ret1.is_some(),
            "Must return a block when one is stored to DB"
        );
        assert_eq!(
            mock_block_1,
            ret1.unwrap(),
            "Returned block must match the one inserted"
        );

        // Inserted a new block and verify that both blocks can be found
        let mock_block_2 = make_mock_block(
            mock_block_1.clone(),
            Some(mock_block_1.header.proof_of_work_family),
        );
        add_block_to_archival_state(&archival_state, mock_block_2.clone()).await?;
        let fetched2 = archival_state.get_block(mock_block_2.hash).await?.unwrap();
        println!("\n\nheight2: {}\n\n", fetched2.header.height);
        assert_eq!(
            mock_block_2, fetched2,
            "Returned block must match the one inserted"
        );
        let fetched1 = archival_state.get_block(mock_block_1.hash).await?.unwrap();
        println!("\n\nheight1: {}\n\n", fetched1.header.height);
        assert_eq!(
            mock_block_1, fetched1,
            "Returned block must match the one inserted"
        );

        // Insert N new blocks and verify that they can all be fetched
        let mut last_block = mock_block_2.clone();
        let mut blocks = vec![genesis, mock_block_1, mock_block_2];
        for _ in 0..(thread_rng().next_u32() % 20) {
            let new_block = make_mock_block(last_block, None);
            add_block_to_archival_state(&archival_state, new_block.clone()).await?;
            blocks.push(new_block.clone());
            last_block = new_block;
        }

        for block in blocks {
            assert_eq!(block, archival_state.get_block(block.hash).await?.unwrap());
        }

        Ok(())
    }

    #[should_panic]
    #[traced_test]
    #[tokio::test]
    async fn digest_of_ancestors_panic_test() {
        let (block_databases, _, root_data_dir_path) = databases(Network::Main).unwrap();
        let archival_state = ArchivalState::new(block_databases, root_data_dir_path);
        let genesis = archival_state.genesis_block.clone();
        archival_state
            .get_ancestor_block_digests(genesis.header.prev_block_digest, 10)
            .await;
    }

    #[traced_test]
    #[tokio::test]
    async fn digest_of_ancestors_test() -> Result<()> {
        let (block_databases, _, root_data_dir_path) = databases(Network::Main).unwrap();
        let archival_state = ArchivalState::new(block_databases, root_data_dir_path);
        let genesis = *archival_state.genesis_block.clone();

        assert!(archival_state
            .get_ancestor_block_digests(genesis.hash, 10)
            .await
            .is_empty());
        assert!(archival_state
            .get_ancestor_block_digests(genesis.hash, 1)
            .await
            .is_empty());

        // Insert blocks and verify that the same result is returned
        let mock_block_1 = make_mock_block(genesis.clone(), None);
        add_block_to_archival_state(&archival_state, mock_block_1.clone()).await?;
        let mock_block_2 = make_mock_block(mock_block_1.clone(), None);
        add_block_to_archival_state(&archival_state, mock_block_2.clone()).await?;
        let mock_block_3 = make_mock_block(mock_block_2.clone(), None);
        add_block_to_archival_state(&archival_state, mock_block_3.clone()).await?;
        let mock_block_4 = make_mock_block(mock_block_3.clone(), None);
        add_block_to_archival_state(&archival_state, mock_block_4.clone()).await?;

        assert!(archival_state
            .get_ancestor_block_digests(genesis.hash, 10)
            .await
            .is_empty());
        assert!(archival_state
            .get_ancestor_block_digests(genesis.hash, 1)
            .await
            .is_empty());

        // Check that ancestors of block 1 and 2 return the right values
        let ancestors_of_1 = archival_state
            .get_ancestor_block_digests(mock_block_1.hash, 10)
            .await;
        assert_eq!(1, ancestors_of_1.len());
        assert_eq!(genesis.hash, ancestors_of_1[0]);

        let ancestors_of_2 = archival_state
            .get_ancestor_block_digests(mock_block_2.hash, 10)
            .await;
        assert_eq!(2, ancestors_of_2.len());
        assert_eq!(mock_block_1.hash, ancestors_of_2[0]);
        assert_eq!(genesis.hash, ancestors_of_2[1]);

        // Verify that max length is respected
        let ancestors_of_4_long = archival_state
            .get_ancestor_block_digests(mock_block_4.hash, 10)
            .await;
        assert_eq!(4, ancestors_of_4_long.len());
        assert_eq!(mock_block_3.hash, ancestors_of_4_long[0]);
        assert_eq!(mock_block_2.hash, ancestors_of_4_long[1]);
        assert_eq!(mock_block_1.hash, ancestors_of_4_long[2]);
        assert_eq!(genesis.hash, ancestors_of_4_long[3]);
        let ancestors_of_4_short = archival_state
            .get_ancestor_block_digests(mock_block_4.hash, 2)
            .await;
        assert_eq!(2, ancestors_of_4_short.len());
        assert_eq!(mock_block_3.hash, ancestors_of_4_short[0]);
        assert_eq!(mock_block_2.hash, ancestors_of_4_short[1]);

        Ok(())
    }
}
