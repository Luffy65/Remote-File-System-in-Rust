// main.rs: parse CLI args, mount point

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use libc::ENOENT; // Errore "No such file or directory"
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH, SystemTime}; // Per i timestamp
use libc::{getuid, getgid}; // Aggiunto per getuid e getgid

const TTL: Duration = Duration::from_secs(1); // Cache per gli attributi

struct RemoteFs {
    server_url: String,
    http_client: reqwest::blocking::Client, // O reqwest::Client se usi async
    // Aggiungi qui un tokio::runtime::Runtime se usi async con block_on
    // runtime: tokio::runtime::Runtime,
}

impl Filesystem for RemoteFs {
    // Implementa qui i metodi del trait Filesystem.
    // Inizia con quelli fondamentali: init, getattr, lookup, readdir.

    fn init(
        &mut self,
        _req: &Request<'_>,
        _config: &mut KernelConfig, // Aggiunto il parametro KernelConfig
    ) -> Result<(), i32> {
        log::info!("Filesystem initialized. Server URL: {}", self.server_url);
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        log::info!("lookup: parent_ino={}, name={:?}", parent, name);
        // Per ora, diciamo che non trova nulla
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        log::info!("getattr: ino={}", ino);
        
        // Per l'inode radice (1), restituisci attributi di directory
        if ino == 1 { // Convenzionalmente, l'inode della root è 1
            let now = SystemTime::now();
            let attr = FileAttr {
                ino: 1,
                size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind: FileType::Directory,
                perm: 0o755, // Permessi rwxr-xr-x
                nlink: 2, // Numero di link (almeno . e ..)
                uid: unsafe { getuid() }, // uid dell'utente corrente
                gid: unsafe { getgid() }, // gid dell'utente corrente
                rdev: 0,
                flags: 0,
                blksize: 512, // Dimensione del blocco
            };
            reply.attr(&TTL, &attr);
        } else {
            reply.error(ENOENT); // Per ora, non conosciamo altri inode
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        log::info!("readdir: ino={}, offset={}", ino, offset);
        if ino == 1 { // Se stiamo leggendo la directory radice
            if offset == 0 {
                // Aggiungi le voci standard "." e ".."
                reply.add(1, 0, FileType::Directory, "."); // Inode 1, offset 0
                reply.add(1, 1, FileType::Directory, ".."); // Inode 1, offset 1
                // Qui poi aggiungerai le voci dal server
            }
            reply.ok();
        } else {
            reply.error(ENOENT);
        }
    }

    // ... e via via gli altri metodi come open, read, write, mkdir, ecc.
}

use std::env;

fn main() {
    env_logger::init(); // Inizializza il logger

    // il primo argomento è il punto di mount
    let mountpoint = match env::args_os().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("Usage: {} <MOUNTPOINT> [SERVER_URL]", env::args().next().unwrap());
            return;
        }
    };

    // il secondo argomento è l'URL del server
    let server_url = env::args().nth(2)
        .unwrap_or_else(|| "http://localhost:3000".to_string()); // URL di default del server

    log::info!("Mounting to {:?}, server URL: {}", mountpoint, server_url);

    // Verifica se il punto di mount esiste
    let fs = RemoteFs {
        server_url,
        http_client: reqwest::blocking::Client::new(),
        // runtime: tokio::runtime::Runtime::new().unwrap(), // Se usi async/block_on
    };

    let options = vec![
        MountOption::FSName("remoteFS".to_string()),
        MountOption::AutoUnmount, // Smonta automaticamente quando il processo termina
        MountOption::AllowRoot,   // Permetti anche a root di accedere (opzionale, valuta la sicurezza)
        // MountOption::RW, // Già di default, ma per esplicitarlo
    ];

    // Esegui il mount
    match fuser::mount2(fs, &mountpoint, &options) {
        Ok(_) => {
            log::info!("Filesystem mounted successfully. Press Ctrl-C to unmount.");
            // Il mount2 blocca finché non viene smontato.
            // Se vuoi che il processo principale continui, dovrai eseguirlo in un thread separato.
            // Per un demone, questo andrebbe gestito diversamente (es. con `fork` o librerie apposite).
        }
        Err(e) => {
            log::error!("Failed to mount filesystem: {}", e);
        }
    }
}
