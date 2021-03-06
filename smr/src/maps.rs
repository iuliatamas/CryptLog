extern crate rustc_serialize;
use self::rustc_serialize::json;
use self::rustc_serialize::{Encodable, Decodable, Encoder, Decoder};

use runtime::Runtime;
use indexed_queue::{Operation, IndexedQueue, State, LogOp};
use encryptors::{MetaEncryptor, Encrypted, Eqable, Ordable};
use converters::{SimpleConverter, Converter, EqableConverter, ConvertersLib};

use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard};
use std::collections::{HashMap, BTreeMap};
use std::hash::Hash;
use std::cmp::Eq;

#[derive(RustcEncodable, RustcDecodable, Debug)]
pub enum MapOp<K, V> {
    Insert {
        key: K,
        val: V,
    },
}

// Unencrypted StringHMap, to be used by client
// Supports Eqable encryption for keys, AES encryption for values
pub type StringHMap<Q> = HMap<String, String, Q>;
impl<Q> StringHMap<Q> {
    pub fn new(aruntime: &Arc<Mutex<Runtime<Q>>>,
               obj_id: i32,
               data: HashMap<String, String>)
               -> StringHMap<Q> {
        HMap::from(aruntime,
                   obj_id,
                   data,
                   Converter::new(ConvertersLib::encodable_from_encrypted(),
                                  ConvertersLib::encrypted_from_encodable()),
                   EqableConverter::new(ConvertersLib::encodable_from_eqable(),
                                        ConvertersLib::eqable_from_encodable()))
    }
}

// Encrypted StringHMap, to be used by VM
// Supports Eqable encryption for keys, AES encryption for values
pub type EncHMap<Q> = HMap<Eqable, Encrypted, Q>;
impl<Q> EncHMap<Q> {
    pub fn new(aruntime: &Arc<Mutex<Runtime<Q>>>,
               obj_id: i32,
               data: HashMap<Eqable, Encrypted>)
               -> EncHMap<Q> {
        HMap::from(aruntime,
                   obj_id,
                   data,
                   Converter::new(ConvertersLib::encrypted_from_encrypted(),
                                  ConvertersLib::encrypted_from_encrypted()),
                   EqableConverter::new(ConvertersLib::eqable_from_eqable(),
                                        ConvertersLib::eqable_from_eqable()))
    }
}

// Class: HMap
// Parametrized by:
// * K : key type
// * V : value type
// * Q : structure allowing seamless communicating with Shared Log
#[derive(Clone)]
pub struct HMap<K, V, Q> {
    runtime: Option<Arc<Mutex<Runtime<Q>>>>, // runtime object is registered with
    obj_id: i32, // unique id
    pub data: Arc<Mutex<HashMap<K, V>>>, // local data structure

    convert_eq: Option<EqableConverter<K>>, // converter between data states
    convert: Option<Converter<V>>, // convert between data states
    secure: Option<MetaEncryptor>, // structure to allow use of existing Encryptors/ Decryptors
}

impl<K, V, Q> Decodable for HMap<K, V, Q>
    where K: Encodable + Decodable + Hash + Eq,
          V: Encodable + Decodable
{
    fn decode<D: Decoder>(d: &mut D) -> Result<Self, D::Error> {
        let mut vec: Vec<(K, V)> = try!(Decodable::decode(d));
        let mut data: HashMap<K, V> = HashMap::new();
        for (k, v) in vec.drain(..) {
            data.insert(k, v);
        }
        let hmap: HMap<K, V, Q> = HMap::default(data);
        let res: Result<Self, D::Error> = Ok(hmap);
        return res;
    }
}

impl<K, V, Q> Encodable for HMap<K, V, Q>
    where K: Encodable + Decodable + Hash + Eq + Clone,
          V: Encodable + Decodable + Clone
{
    fn encode<S: Encoder>(&self, s: &mut S) -> Result<(), S::Error> {
        let data = self.data.lock().unwrap();
        let mut vec: Vec<(K, V)> = Vec::new();
        for (k, v) in data.iter() {
            vec.push((k.clone(), v.clone()));
        }
        vec.encode(s)
    }
}

impl<K, V, Q> HMap<K, V, Q> {
    pub fn from(aruntime: &Arc<Mutex<Runtime<Q>>>,
                obj_id: i32,
                data: HashMap<K, V>,
                convert: Converter<V>,
                convert_eq: EqableConverter<K>)
                -> HMap<K, V, Q> {
        let hmap = HMap {
            obj_id: obj_id,
            runtime: Some(aruntime.clone()),
            data: Arc::new(Mutex::new(data)),
            convert: Some(convert),
            convert_eq: Some(convert_eq),
            secure: aruntime.lock().unwrap().secure.clone(),
        };
        return hmap;
    }

    fn default(data: HashMap<K, V>) -> HMap<K, V, Q> {
        HMap {
            runtime: None,
            obj_id: 0,
            data: Arc::new(Mutex::new(data)),
            convert: None,
            convert_eq: None,
            secure: None,
        }
    }
}

impl<K, V, Q> HMap<K, V, Q>
    where K: 'static + Send + Clone + Encodable + Decodable + Hash + Eq,
          V: 'static + Send + Clone + Encodable + Decodable,
          Q: 'static + IndexedQueue + Send + Clone
{
    // lock runtime, call f with runtime, release lock
    fn with_runtime<R, T, F>(&self, f: F) -> T
        where F: FnOnce(MutexGuard<Runtime<Q>>) -> T
    {
        assert!(self.runtime.is_some(), "invalid runtime");
        self.runtime
            .as_ref()
            .map(|runtime| {
                let runtime = runtime.lock().unwrap();
                f(runtime)
            })
            .unwrap()
    }

    pub fn start(&mut self) {
        self.with_runtime::<(), _, _>(|mut runtime| {
            let mut obj = self.clone();
            runtime.register_object(self.obj_id,
                                    Box::new(move |_, op: Operation| obj.callback(op)));

        });
    }

    pub fn get(&self, k: &K) -> Option<V> {
        self.with_runtime::<V, _, _>(|mut runtime| {
            runtime.sync(Some(self.obj_id));
            let data = self.data.lock().unwrap();
            data.get(k).cloned()
        })
    }

    pub fn insert(&mut self, k: K, v: V) {
        self.with_runtime::<(), _, _>(|mut runtime| {
            // convert key and value to shared log state
            let key = self.convert_eq
                          .as_ref()
                          .map(|convert_eq| {
                              let to = &convert_eq.to;
                              to(&self.secure, k)
                          })
                          .unwrap();
            let val = self.convert
                          .as_ref()
                          .map(|convert| {
                              let to = &convert.to;
                              to(&self.secure, v)
                          })
                          .unwrap();
            let encrypted_op = MapOp::Insert {
                key: key,
                val: val,
            };
            let op = json::encode(&encrypted_op).unwrap();
            runtime.append(self.obj_id, State::Encrypted(op.into_bytes()));
        });
    }

    pub fn get_val(&self, val: Encrypted) -> V {
        // convert value from shared log state to local state
        self.convert
            .as_ref()
            .map(|convert| {
                let from = &convert.from;
                from(&self.secure, val)
            })
            .unwrap()
    }

    pub fn get_key(&self, key: Eqable) -> K {
        // convert key from shared log state to local state
        self.convert_eq
            .as_ref()
            .map(|convert_eq| {
                let from = &convert_eq.from;
                from(&self.secure, key)
            })
            .unwrap()
    }

    pub fn callback(&mut self, op: Operation) {
        match op.operator {
            LogOp::Op(State::Encrypted(ref s)) => {
                let encrypted_op = json::decode(&String::from_utf8(s.clone()).unwrap()).unwrap();
                match encrypted_op {
                    MapOp::Insert{key: k, val: v} => {
                        let k = self.get_key(k);
                        let v = self.get_val(v);
                        let mut m_data = self.data.lock().unwrap();
                        m_data.insert(k, v);
                    }
                }
            }
            LogOp::Snapshot(State::Encoded(ref s)) => {
                let obj: HMap<Eqable, Encrypted, Q> = json::decode(&s).unwrap();
                let mut converted: HashMap<K, V> = HashMap::new();
                let data = obj.data.lock().unwrap();
                for (k, v) in data.iter() {
                    converted.insert(self.get_key(k.clone()), self.get_val(v.clone()));
                }
                *self.data.lock().unwrap() = converted;
            }
            _ => {
                unimplemented!();
            }
        }
    }
}

// Unencrypted StringBTMap, to be used by client
// Supports Ordable encryption for keys, AES encryption for values
pub type StringBTMap<Q> = BTMap<String, String, Q, Ordable, Encrypted>;
impl<Q> StringBTMap<Q> {
    pub fn new(aruntime: &Arc<Mutex<Runtime<Q>>>,
               obj_id: i32,
               data: BTreeMap<String, String>)
               -> StringBTMap<Q> {
        BTMap::from(aruntime,
                    obj_id,
                    data,
                    SimpleConverter::new(ConvertersLib::encodable_from_encrypted(),
                                         ConvertersLib::encrypted_from_encodable()),
                    SimpleConverter::new(ConvertersLib::encodable_from_ordable(),
                                         ConvertersLib::ordable_from_encodable()))
    }
}

// Encrypted StringBTMap, to be used by VM
// Supports Ordable encryption for keys, AES encryption for values
pub type EncBTMap<Q> = BTMap<Ordable, Encrypted, Q, Ordable, Encrypted>;
impl<Q> EncBTMap<Q> {
    pub fn new(aruntime: &Arc<Mutex<Runtime<Q>>>,
               obj_id: i32,
               data: BTreeMap<Ordable, Encrypted>)
               -> EncBTMap<Q> {
        BTMap::from(aruntime,
                    obj_id,
                    data,
                    SimpleConverter::new(ConvertersLib::encrypted_from_encrypted(),
                                         ConvertersLib::encrypted_from_encrypted()),
                    SimpleConverter::new(ConvertersLib::ordable_from_ordable(),
                                         ConvertersLib::ordable_from_ordable()))
    }
}

// Unencrypted StringBTMap, to be used by benchmark
// Encryption is replaced by identity function
pub type UnencBTMap<Q> = BTMap<String, String, Q, String, String>;
impl<Q> UnencBTMap<Q> {
    pub fn new(aruntime: &Arc<Mutex<Runtime<Q>>>,
               obj_id: i32,
               data: BTreeMap<String, String>)
               -> UnencBTMap<Q> {
        BTMap::from(aruntime,
                    obj_id,
                    data,
                    SimpleConverter::new(ConvertersLib::encodable_from_encodable(),
                                         ConvertersLib::encodable_from_encodable()),
                    SimpleConverter::new(ConvertersLib::encodable_from_encodable(),
                                         ConvertersLib::encodable_from_encodable()))
    }
}


// Class: BTMap
// Parametrized by:
// * K : key type
// * V : value type
// * Q : structure allowing seamless communicating with Shared Log
// * KE : encrypted key type
// * VE : encrypted value type
#[derive(Clone)]
pub struct BTMap<K, V, Q, KE, VE> {
    runtime: Option<Arc<Mutex<Runtime<Q>>>>,
    obj_id: i32,

    convert_ord: Option<SimpleConverter<K, KE>>,
    convert: Option<SimpleConverter<V, VE>>,

    secure: Option<MetaEncryptor>,
    pub data: Arc<Mutex<BTreeMap<K, V>>>,
}

impl<K, V, Q, KE, VE> Decodable for BTMap<K, V, Q, KE, VE>
    where K: Encodable + Decodable + Ord,
          V: Encodable + Decodable,
          KE: Encodable + Decodable + Ord,
          VE: Encodable + Decodable
{
    fn decode<D: Decoder>(d: &mut D) -> Result<Self, D::Error> {
        let data = try!(Decodable::decode(d));
        let btmap: BTMap<K, V, Q, KE, VE> = BTMap::default(data);
        let res: Result<Self, D::Error> = Ok(btmap);
        return res;
    }
}

impl<K, V, Q, KE, VE> Encodable for BTMap<K, V, Q, KE, VE>
    where K: Encodable + Decodable + Ord,
          V: Encodable + Decodable,
          KE: Encodable + Decodable + Ord,
          VE: Encodable + Decodable
{
    fn encode<S: Encoder>(&self, s: &mut S) -> Result<(), S::Error> {
        let data = self.data.lock().unwrap();
        data.encode(s)
    }
}

impl<K, V, Q, KE, VE> BTMap<K, V, Q, KE, VE> {
    pub fn from(aruntime: &Arc<Mutex<Runtime<Q>>>,
                obj_id: i32,
                data: BTreeMap<K, V>,
                convert: SimpleConverter<V, VE>,
                convert_ord: SimpleConverter<K, KE>)
                -> BTMap<K, V, Q, KE, VE> {
        let btmap = BTMap {
            obj_id: obj_id,
            runtime: Some(aruntime.clone()),
            secure: aruntime.lock().unwrap().secure.clone(),
            data: Arc::new(Mutex::new(data)),
            convert: Some(convert),
            convert_ord: Some(convert_ord),
        };
        return btmap;
    }

    fn default(data: BTreeMap<K, V>) -> BTMap<K, V, Q, KE, VE> {
        BTMap {
            obj_id: 0,
            runtime: None,
            secure: None,
            data: Arc::new(Mutex::new(data)),
            convert: None,
            convert_ord: None,
        }
    }
}

impl<K, V, Q, KE, VE> BTMap<K, V, Q, KE, VE>
    where K: 'static + Ord + Send + Clone + Encodable + Decodable + Debug,
          V: 'static + Send + Clone + Encodable + Decodable + Debug,
          Q: 'static + IndexedQueue + Send + Clone,
          KE: 'static + Ord + Send + Clone + Encodable + Decodable + Debug,
          VE: 'static + Send + Clone + Encodable + Decodable + Debug
{
    // lock runtime, call f with runtime, release lock
    fn with_runtime<R, T, F>(&self, f: F) -> T
        where F: FnOnce(MutexGuard<Runtime<Q>>) -> T
    {
        assert!(self.runtime.is_some(), "invalid runtime");
        self.runtime
            .as_ref()
            .map(|runtime| {
                let runtime = runtime.lock().unwrap();
                f(runtime)
            })
            .unwrap()
    }

    pub fn start(&mut self) {
        self.with_runtime::<(), _, _>(|mut runtime| {
            let mut obj = self.clone();
            runtime.register_object(self.obj_id,
                                    Box::new(move |_, op: Operation| obj.callback(op)));

        });
    }

    pub fn get(&self, k: &K) -> Option<V> {
        self.with_runtime::<V, _, _>(|mut runtime| {
            runtime.sync(Some(self.obj_id));
            let data = self.data.lock().unwrap();
            data.get(k).cloned()
        })
    }

    // use only for testing map is in order
    // modfies local map without syncing to log
    pub fn pop_first(&mut self) -> Option<(K, V)> {
        self.with_runtime::<K, _, _>(|mut runtime| {
            runtime.sync(Some(self.obj_id));
            // println!("synced!");
            {
                let data = self.data.lock().unwrap();
                if data.is_empty() {
                    return None;
                }
            }

            let res = {
                let data = self.data.lock().unwrap();
                let (first_key, first_value) = data.iter().next().unwrap();
                (first_key.clone(), first_value.clone())
            };

            self.data.lock().unwrap().remove(&res.0);
            Some(res)
        })
    }

    pub fn get_val(&self, val: VE) -> V {
        // convert value from shared log state to local state
        self.convert
            .as_ref()
            .map(|convert| {
                let from = &convert.from;
                from(&self.secure, val)
            })
            .unwrap()
    }

    pub fn get_key(&self, key: KE) -> K {
        // convert key from shared log state to local state
        self.convert_ord
            .as_ref()
            .map(|convert_ord| {
                let from = &convert_ord.from;
                from(&self.secure, key)
            })
            .unwrap()
    }

    pub fn insert(&mut self, k: K, v: V) {
        self.with_runtime::<(), _, _>(|mut runtime| {
            // convert key and value to shared log state
            let key = self.convert_ord
                          .as_ref()
                          .map(|convert_ord| {
                              let to = &convert_ord.to;
                              to(&self.secure, k)
                          })
                          .unwrap();
            let val = self.convert
                          .as_ref()
                          .map(|convert| {
                              let to = &convert.to;
                              to(&self.secure, v)
                          })
                          .unwrap();
            let encrypted_op = MapOp::Insert {
                key: key,
                val: val,
            };
            let op = json::encode(&encrypted_op).unwrap();
            runtime.append(self.obj_id, State::Encrypted(op.into_bytes()));
        });
    }

    pub fn callback(&mut self, op: Operation) {
        match op.operator {
            LogOp::Op(State::Encrypted(ref s)) => {
                let encrypted_op = json::decode(&String::from_utf8(s.clone()).unwrap()).unwrap();
                match encrypted_op {
                    MapOp::Insert{key: k, val: v} => {
                        let k = self.get_key(k);
                        let v = self.get_val(v);
                        let mut m_data = self.data.lock().unwrap();
                        m_data.insert(k, v);
                    }
                }
            }
            LogOp::Snapshot(State::Encoded(ref s)) => {
                let mut obj: BTreeMap<KE, VE> = json::decode(&s).unwrap();
                let mut converted = BTreeMap::new();
                for (k, v) in obj.iter_mut() {
                    converted.insert(self.get_key(k.clone()), self.get_val(v.clone()));
                }
                *self.data.lock().unwrap() = converted;
            }
            _ => {
                unimplemented!();
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{StringHMap, StringBTMap, UnencBTMap};
    use std::collections::{HashMap, BTreeMap};
    use std::char;
    use std::sync::{Arc, Mutex};
    use runtime::Runtime;
    use indexed_queue::InMemoryQueue;
    use encryptors::MetaEncryptor;
    use converters::{Converter, ConvertersLib, EqableConverter};

    #[test]
    fn hmap_read_write() {
        let q = InMemoryQueue::new();
        let runtime: Runtime<InMemoryQueue> = Runtime::new(q, Some(MetaEncryptor::new()));
        let aruntime = Arc::new(Mutex::new(runtime));
        let n = 5;
        let obj_id = 1;
        let _: Converter<String> = Converter::new(ConvertersLib::encodable_from_encrypted(),
                                                  ConvertersLib::encrypted_from_encodable());
        let _: EqableConverter<i32> = EqableConverter::new(ConvertersLib::encodable_from_eqable(),
                                                           ConvertersLib::eqable_from_encodable());
        let mut hmap = StringHMap::new(&aruntime, obj_id, HashMap::new());
        hmap.start();

        for key in 0..n {
            let mut val = String::from("hello_");
            val.push(char::from_u32(key as u32).unwrap());
            let key2: String = key.to_string();
            hmap.insert(key2.clone(), val.clone());
            assert_eq!(val, hmap.get(&key2).unwrap());
        }

        assert!(hmap.runtime.is_some(), "invalid runtime");
        hmap.runtime.map(|runtime| {
            assert_eq!(runtime.lock().unwrap().global_idx, (n - 1) as i64);
        });

    }

    #[test]
    fn btmap_read_write() {
        let q = InMemoryQueue::new();
        let runtime: Runtime<InMemoryQueue> = Runtime::new(q, Some(MetaEncryptor::new()));
        let aruntime = Arc::new(Mutex::new(runtime));
        let n = 5;
        let obj_id = 1;
        let mut btmap = StringBTMap::new(&aruntime, obj_id, BTreeMap::new());
        btmap.start();

        let keys = vec!["h0", "h1", "h2", "alphabet", "h0rry"];
        let vals = vec!["h0", "h1", "h2", "alphabet", "h0rry"];
        let should_be_at = vec![3, 0, 4, 1, 2];
        for i in 0..keys.len() {
            btmap.insert(String::from(keys[i].clone()), String::from(vals[i].clone()));
            assert_eq!(vals[i], btmap.get(&String::from(keys[i])).unwrap());
        }

        assert!(btmap.runtime.is_some(), "invalid runtime");
        btmap.runtime = btmap.runtime
                             .map(|runtime| {
                                 assert_eq!(runtime.lock().unwrap().global_idx, (n - 1) as i64);
                                 runtime
                             });

        for i in 0..keys.len() {
            let (_, val) = btmap.pop_first().unwrap();
            // println!("key {:?} val {:?}", key, val);
            assert_eq!(val, vals[should_be_at[i]]);
        }

    }

    #[test]
    fn btmap_unec() {
        let q = InMemoryQueue::new();
        let runtime: Runtime<InMemoryQueue> = Runtime::new(q, Some(MetaEncryptor::new()));
        let aruntime = Arc::new(Mutex::new(runtime));
        let n = 5;
        let obj_id = 1;
        let mut btmap = UnencBTMap::new(&aruntime, obj_id, BTreeMap::new());
        btmap.start();

        let keys = vec!["h0", "h1", "h2", "alphabet", "h0rry"];
        let vals = vec!["h0", "h1", "h2", "alphabet", "h0rry"];
        let should_be_at = vec![3, 0, 4, 1, 2];
        for i in 0..keys.len() {
            btmap.insert(String::from(keys[i].clone()), String::from(vals[i].clone()));
            assert_eq!(vals[i], btmap.get(&String::from(keys[i])).unwrap());
        }

        assert!(btmap.runtime.is_some(), "invalid runtime");
        btmap.runtime = btmap.runtime
                             .map(|runtime| {
                                 assert_eq!(runtime.lock().unwrap().global_idx, (n - 1) as i64);
                                 runtime
                             });

        for i in 0..keys.len() {
            let (_, val) = btmap.pop_first().unwrap();
            // println!("key {:?} val {:?}", key, val);
            assert_eq!(val, vals[should_be_at[i]]);
        }

    }
}
