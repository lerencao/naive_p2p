use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::iter::FromIterator;

pub struct NodeState {
    data: BTreeMap<u32, Vec<u8>>,
    cur_state: Option<(u32, Vec<u8>)>,
}
impl Default for NodeState {
    fn default() -> Self {
        NodeState::new()
    }
}

impl NodeState {
    pub fn new() -> NodeState {
        NodeState {
            data: BTreeMap::new(),
            cur_state: None,
        }
    }

    pub fn contains(&self, index: &u32) -> bool {
        self.data.contains_key(index)
    }

    pub fn scan_block(&self, start_index: u32, max_len: u32) -> Vec<(u32, Vec<u8>)> {
        let iter = self
            .data
            .range(start_index..u32::max_value())
            .take(max_len as usize);
        Vec::from_iter(iter.map(|(index, v)| (*index, v.to_vec())))
    }

    /// insert a data,
    /// and try advance hashing block, return lastest nonce and hash
    pub fn insert(&mut self, index: u32, data: Vec<u8>) -> Option<(u32, Vec<u8>)> {
        if self.data.contains_key(&index) {
            return self.cur_state.clone();
        }
        let old = self.data.insert(index, data);
        assert!(old.is_none());

        self.try_advance();
        self.cur_state.clone()
    }

    pub fn cur_state(&self) -> Option<(&u32, &[u8])> {
        match self.cur_state {
            Some((ref index, ref hash)) => Some((index, hash)),
            None => None,
        }
    }

    pub fn lastest_block_index(&self) -> Option<u32> {
        match self.data.iter().rev().next() {
            Some((index, _)) => Some(*index),
            None => None,
        }
    }

    fn try_advance(&mut self) {
        let mut last_state = self.cur_state.clone();
        loop {
            let next_index = match last_state {
                Some(ref t) => t.0 + 1,
                None => 0,
            };
            let data = match self.data.get(&next_index) {
                Some(data) => data,
                None => {
                    break;
                }
            };
            let next_hash = match last_state {
                Some((_, ref hash)) => {
                    let mut to_be_hashed = hash.clone();
                    to_be_hashed.extend_from_slice(data);
                    sha256(to_be_hashed)
                }
                None => sha256(data),
            };
            last_state = Some((next_index, next_hash));
        }
        self.cur_state = last_state;
    }
}

fn sha256<B: AsRef<[u8]>>(data: B) -> Vec<u8> {
    // create a Sha256 object
    let mut hasher = Sha256::new();

    // write input message
    hasher.input(data);

    // read hash digest and consume hasher
    let result = hasher.result();
    result.to_vec()
}

#[cfg(test)]
mod test {
    use super::NodeState;
    #[test]
    pub fn test_state_insert() {
        let mut state = NodeState::new();

        {
            let cur_state = state.insert(1, vec![0, 1]);
            assert!(cur_state.is_none());
        }

        {
            let cur_state = state.insert(1, vec![0, 1, 2]);
            assert!(cur_state.is_none());
        }

        {
            let cur_state = state.insert(0, vec![0]);
            assert!(cur_state.is_some());
            let cur_state = cur_state.unwrap();
            assert_eq!(1, cur_state.0);
        }

        {
            let cur_state = state.insert(2, vec![0, 1, 2]);
            assert!(cur_state.is_some());
            let cur_state = cur_state.unwrap();
            assert_eq!(2, cur_state.0);
        }
    }
}
