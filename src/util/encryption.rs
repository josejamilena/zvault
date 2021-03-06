use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::io;
use std::fs::{self, File};
use std::sync::{Once, ONCE_INIT};

use serde_yaml;
use serde_bytes::ByteBuf;

use libsodium_sys;
use sodiumoxide;
use sodiumoxide::crypto::sealedbox;
use sodiumoxide::crypto::box_;
use sodiumoxide::crypto::pwhash;
pub use sodiumoxide::crypto::box_::{SecretKey, PublicKey};

use ::util::*;


static INIT: Once = ONCE_INIT;

fn sodium_init() {
    INIT.call_once(|| {
        if !sodiumoxide::init() {
            panic!("Failed to initialize sodiumoxide");
        }
    });
}

quick_error!{
    #[derive(Debug)]
    pub enum EncryptionError {
        InvalidKey {
            description("Invalid key")
        }
        MissingKey(key: PublicKey) {
            description("Missing key")
            display("Missing key: {}", to_hex(&key[..]))
        }
        Operation(reason: &'static str) {
            description("Operation failed")
            display("Operation failed: {}", reason)
        }
        Io(err: io::Error) {
            from()
            cause(err)
            description("IO error")
            display("IO error: {}", err)
        }
        Yaml(err: serde_yaml::Error) {
            from()
            cause(err)
            description("Yaml format error")
            display("Yaml format error: {}", err)
        }
    }
}


#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[allow(unknown_lints,non_camel_case_types)]
pub enum EncryptionMethod {
    Sodium,
}
serde_impl!(EncryptionMethod(u64) {
    Sodium => 0
});

impl EncryptionMethod {
    pub fn from_string(val: &str) -> Result<Self, &'static str> {
        match val {
            "sodium" => Ok(EncryptionMethod::Sodium),
            _ => Err("Unsupported encryption method")
        }
    }

    pub fn to_string(&self) -> String {
        match *self {
            EncryptionMethod::Sodium => "sodium".to_string()
        }
    }
}


pub type Encryption = (EncryptionMethod, ByteBuf);


struct KeyfileYaml {
    public: String,
    secret: String
}
impl Default for KeyfileYaml {
    fn default() -> Self {
        KeyfileYaml {
            public: "".to_string(),
            secret: "".to_string()
        }
    }
}
serde_impl!(KeyfileYaml(String) {
    public: String => "public",
    secret: String => "secret"
});

impl KeyfileYaml {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, EncryptionError> {
        let f = try!(File::open(path));
        Ok(try!(serde_yaml::from_reader(f)))
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), EncryptionError> {
        let mut f = try!(File::create(path));
        Ok(try!(serde_yaml::to_writer(&mut f, &self)))
    }
}


pub struct Crypto {
    path: PathBuf,
    keys: HashMap<PublicKey, SecretKey>
}

impl Crypto {
    #[inline]
    pub fn dummy() -> Self {
        sodium_init();
        Crypto { path: PathBuf::new(), keys: HashMap::new() }
    }

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EncryptionError> {
        sodium_init();
        let path = path.as_ref().to_owned();
        let mut keys: HashMap<PublicKey, SecretKey> = HashMap::default();
        for entry in try!(fs::read_dir(&path)) {
            let entry = try!(entry);
            let keyfile = try!(KeyfileYaml::load(entry.path()));
            let public = try!(parse_hex(&keyfile.public).map_err(|_| EncryptionError::InvalidKey));
            let public = try!(PublicKey::from_slice(&public).ok_or(EncryptionError::InvalidKey));
            let secret = try!(parse_hex(&keyfile.secret).map_err(|_| EncryptionError::InvalidKey));
            let secret = try!(SecretKey::from_slice(&secret).ok_or(EncryptionError::InvalidKey));
            keys.insert(public, secret);
        }
        Ok(Crypto { path: path, keys: keys })
    }

    #[inline]
    pub fn add_secret_key(&mut self, public: PublicKey, secret: SecretKey) {
        self.keys.insert(public, secret);
    }

    #[inline]
    pub fn register_keyfile<P: AsRef<Path>>(&mut self, path: P) -> Result<(), EncryptionError> {
        let (public, secret) = try!(Self::load_keypair_from_file(path));
        self.register_secret_key(public, secret)
    }

    pub fn load_keypair_from_file<P: AsRef<Path>>(path: P) -> Result<(PublicKey, SecretKey), EncryptionError> {
        let keyfile = try!(KeyfileYaml::load(path));
        let public = try!(parse_hex(&keyfile.public).map_err(|_| EncryptionError::InvalidKey));
        let public = try!(PublicKey::from_slice(&public).ok_or(EncryptionError::InvalidKey));
        let secret = try!(parse_hex(&keyfile.secret).map_err(|_| EncryptionError::InvalidKey));
        let secret = try!(SecretKey::from_slice(&secret).ok_or(EncryptionError::InvalidKey));
        Ok((public, secret))
    }

    #[inline]
    pub fn save_keypair_to_file<P: AsRef<Path>>(public: &PublicKey, secret: &SecretKey, path: P) -> Result<(), EncryptionError> {
        KeyfileYaml { public: to_hex(&public[..]), secret: to_hex(&secret[..]) }.save(path)
    }

    #[inline]
    pub fn register_secret_key(&mut self, public: PublicKey, secret: SecretKey) -> Result<(), EncryptionError> {
        let path = self.path.join(to_hex(&public[..]) + ".yaml");
        try!(Self::save_keypair_to_file(&public, &secret, path));
        self.keys.insert(public, secret);
        Ok(())
    }

    #[inline]
    pub fn contains_secret_key(&mut self, public: &PublicKey) -> bool {
        self.keys.contains_key(public)
    }

    fn get_secret_key(&self, public: &PublicKey) -> Result<&SecretKey, EncryptionError> {
        self.keys.get(public).ok_or_else(|| EncryptionError::MissingKey(*public))
    }

    #[inline]
    pub fn encrypt(&self, enc: &Encryption, data: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let &(ref method, ref public) = enc;
        let public = try!(PublicKey::from_slice(public).ok_or(EncryptionError::InvalidKey));
        match *method {
            EncryptionMethod::Sodium => {
                Ok(sealedbox::seal(data, &public))
            }
        }
    }

    #[inline]
    pub fn decrypt(&self, enc: &Encryption, data: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let &(ref method, ref public) = enc;
        let public = try!(PublicKey::from_slice(public).ok_or(EncryptionError::InvalidKey));
        let secret = try!(self.get_secret_key(&public));
        match *method {
            EncryptionMethod::Sodium => {
                sealedbox::open(data, &public, secret).map_err(|_| EncryptionError::Operation("Decryption failed"))
            }
        }
    }

    #[inline]
    pub fn gen_keypair() -> (PublicKey, SecretKey) {
        sodium_init();
        box_::gen_keypair()
    }

    pub fn keypair_from_password(password: &str) -> (PublicKey, SecretKey) {
        let salt = pwhash::Salt::from_slice(b"the_great_zvault_password_salt_1").unwrap();
        let mut key = [0u8; pwhash::HASHEDPASSWORDBYTES];
        let key = pwhash::derive_key(&mut key, password.as_bytes(), &salt, pwhash::OPSLIMIT_INTERACTIVE, pwhash::MEMLIMIT_INTERACTIVE).unwrap();
        let mut seed = [0u8; 32];
        let offset = key.len()-seed.len();
        for (i, b) in seed.iter_mut().enumerate() {
            *b = key[i+offset];
        }
        let mut pk = [0u8; 32];
        let mut sk = [0u8; 32];
        if unsafe { libsodium_sys::crypto_box_seed_keypair(&mut pk, &mut sk, &seed) } != 0 {
            panic!("Libsodium failed");
        }
        (PublicKey::from_slice(&pk).unwrap(), SecretKey::from_slice(&sk).unwrap())
    }
}
