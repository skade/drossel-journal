extern crate leveldb;
extern crate db_key as key;

use leveldb::database::Database;
use leveldb::database::kv::KV;
use leveldb::database::error::Error;
use leveldb::database::comparator::{OrdComparator};
use leveldb::database::iterator::{Iterable};
use leveldb::options::{Options,WriteOptions,ReadOptions};
use std::cmp::Ordering;
use std::path::Path;

#[derive(Debug,PartialEq,Eq,PartialOrd,Ord,Clone,Copy)]
#[repr(u64)]
pub enum KeyType {
  Queue,
  Chunk
}

pub type Id = u64;

#[derive(Debug,PartialEq,Eq,Clone,Copy)]
pub struct Key {
  id: Id,
  keytype: KeyType,
}

impl Key {
  pub fn empty() -> Key {
    Key { keytype: KeyType::Queue, id: 0 }
  }

  pub fn new(keytype: KeyType, id: Id) -> Key {
    Key { keytype: keytype, id: id }
  }
}

impl key::Key for Key {
  fn from_u8(key: &[u8]) -> Key {
    use std::mem::transmute;

    assert!(key.len() == 16);
    let mut result: [u8; 16] = [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0];

    for (i, val) in key.iter().enumerate() {
      result[i] = *val;
    }

    unsafe { transmute(result) }
  }

  fn as_slice<T, F: Fn(&[u8]) -> T>(&self, f: F) -> T {
    use std::mem::transmute;

    let val = unsafe { transmute::<_, &[u8; 16]>(self) };
    f(val)
  }
}

impl PartialOrd for Key {
  fn partial_cmp(&self, other: &Key) -> Option<Ordering> {
    if self.keytype < other.keytype {
      return Some(Ordering::Less)
    }
    if self.keytype > other.keytype {
      return Some(Ordering::Greater)
    }
    if self.id < other.id {
      return Some(Ordering::Less)
    }
    if self.id > other.id {
      return Some(Ordering::Greater)
    }
    None
  }
}

impl Ord for Key {
  fn cmp(&self, other: &Key) -> Ordering {
    if self.keytype < other.keytype {
      return Ordering::Less
    }
    if self.keytype > other.keytype {
      return Ordering::Greater
    }
    if self.id < other.id {
      return Ordering::Less
    }
    if self.id > other.id {
      return Ordering::Greater
    }
    Ordering::Equal
  }
}

pub struct Journal {
  db: Database<Key>,
  head: Key, // The key that points to the last value written
  tail: Key, // The key that points to the earliest value written, but not read
  reserved_tail: Key // The key that points to the beginning of the reserved block
}

impl Journal {
  fn new(path: &Path) -> Result<Journal, Error> {
    let mut options = Options::new();
    options.create_if_missing = true;
    let db = Database::open_with_comparator(path, options, OrdComparator::new("journal-comparator".into()));
    let head = Key { keytype: KeyType::Queue, id: 0 };
    let tail = Key { keytype: KeyType::Queue, id: 0 };
    let reserved_tail = Key { keytype: KeyType::Queue, id: 0 };
    match db {
      Ok(new) => Ok(Journal { db: new, head: head, tail: tail, reserved_tail: reserved_tail }),
      Err(e) => Err(e)
    }
  }

  fn open_existing(path: &Path) -> Result<Journal,Error> {
    let mut options = Options::new();
    options.create_if_missing = false;
    let db = Database::open_with_comparator(path, options, OrdComparator::new("journal-comparator".into()));
    match db {
      Ok(mut existing) => {
        let (head, tail, reserved_tail) = Journal::read_keys(&mut existing);
        Ok(Journal { db: existing, head: head, tail: tail, reserved_tail: reserved_tail })
      },
      Err(e) => Err(e)
    }
  }

  fn read_keys<'a>(db: &'a Database<Key>) -> (Key, Key, Key) {
    let read_options = ReadOptions::new();
    let mut iter = db.keys_iter(read_options);
    let reserved_tail = Key { keytype: KeyType::Queue, id: 0 };
    if let Some(first) = iter.next() {
      let tail = first;
      if let Some(_) = iter.next() {
        let last = iter.last().unwrap();
        let head = last;
        (head.clone(), tail.clone(), reserved_tail)
      } else {
        (tail.clone(), tail.clone(), reserved_tail)
      }
    } else {
      // we have a db, but no keys in it
      let queue_head = Key { keytype: KeyType::Queue, id: 0 };
      let queue_tail = Key { keytype: KeyType::Queue, id: 0 };
      (queue_head, queue_tail, reserved_tail)
    }
  }

  pub fn open(path: &Path) -> Result<Journal,Error> {
    let res = Journal::open_existing(path);
    match res {
      Ok(j) => Ok(j),
      Err(_) => {
        Journal::new(path)
      }
    }
  }

  pub fn push(&mut self, data: &[u8]) {
    let mut write_options = WriteOptions::new();
    write_options.sync = true;
    self.db.put(write_options, self.head, data).unwrap_or_else(|err| {
      panic!("error writing to journal: {:?}", err)
    });

    self.head.id = self.head.id + 1;
  }

  pub fn pop(&mut self) -> Option<Vec<u8>> {
    if self.head.id >= self.tail.id {
      let res = self.peek();
      self.remove(false);
      if res.is_some() {
        self.tail.id = self.tail.id + 1;
      }
      return res;
    } else {
      None
    }
  }

  pub fn peek(&self) -> Option<Vec<u8>> {
    if self.head.id >= self.tail.id {
      let read_options = ReadOptions::new();
      let result = self.db.get(read_options, self.tail).unwrap_or_else(|err| {
        panic!("error reading from journal: {:?}", err)
      });
      result
    } else {
      None
    }
  }

  fn remove(&mut self, reserved: bool) {
    let key = if reserved {
                self.tail
              } else {
                self.reserved_tail
              };

    let mut write_options = WriteOptions::new();
    write_options.sync = true;
    self.db.delete(write_options, key).unwrap_or_else(|err| {
      panic!("error reading from journal: {:?}", err)
    });

    if reserved {
      self.advance_to_next_reserved();
    }
  }

  fn advance_to_next_reserved(&mut self) {
    let read_options = ReadOptions::new();
    let database: &Iterable<Key> = &self.db;
    let mut iter = database.keys_iter(read_options);

    if let Some(next_key) = iter.next() {
      self.reserved_tail = next_key.clone();
    }
  }

  pub fn len(&self) -> u64 {
    self.head.id - self.tail.id
  }
}

#[cfg(test)]
mod tests {
  extern crate tempdir;

  use super::{Key,KeyType,Journal};
  use self::tempdir::TempDir;
  use std::cmp::Ordering;

  #[test]
  fn test_compare() {
    let key = Key { keytype: KeyType::Queue, id: 123 };
    let key2 = Key { keytype: KeyType::Chunk, id: 123 };
    let key3 = Key { keytype: KeyType::Queue, id: 124 };
    assert_eq!(Ordering::Less, key.cmp(&key2));
    assert_eq!(Ordering::Greater, key2.cmp(&key));
    assert_eq!(Ordering::Less, key.cmp(&key3));
    assert_eq!(Ordering::Greater, key3.cmp(&key));
    assert_eq!(Ordering::Equal, key.cmp(&key));
  }

  #[test]
  fn test_equality() {
    let key = Key { keytype: KeyType::Queue, id: 0 };
    let key2 = Key { keytype: KeyType::Queue, id: 0 };
    assert_eq!(Ordering::Equal, key.cmp(&key2));
  }

  #[test]
  fn test_push() {
    let dir = TempDir::new("journal_test").unwrap();
    let mut journal = Journal::open(dir.path()).unwrap();
    journal.push(&[1u8]);
    let res = journal.peek();
    assert!(res.is_some());
  }

  #[test]
  fn test_journal() {
    let dir = TempDir::new("journal_test").unwrap();
    let mut journal = Journal::open(dir.path()).unwrap();
    let res = journal.pop();
    assert!(res.is_none());
    journal.push(&[1u8]);
    journal.push(&[2u8]);
    let res2 = journal.pop();
    assert!(res2.is_some());
    assert_eq!(Some(vec![1 as u8]), res2);
    let res3 = journal.pop();
    assert!(res3.is_some());
    assert_eq!(Some(vec![2 as u8]), res3);
    let res4 = journal.pop();
    assert!(res4.is_none());
    assert_eq!(0, journal.len());
  }
}
