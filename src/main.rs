extern crate bodyparser;
extern crate clap;
extern crate iron;
extern crate jfs;
extern crate kafka;
#[macro_use]
extern crate lazy_static;
extern crate openssl;
#[macro_use]
extern crate router;
extern crate rustc_serialize;

#[cfg(feature = "stats-prometheus")]
#[macro_use]
extern crate prometheus;
#[cfg(feature = "stats-statsd")]
extern crate cadence;
#[cfg(feature = "reporter-slack")]
extern crate slack_hook;

mod models;
mod reporter;
mod stats;
mod utils;

use iron::prelude::*;
use iron::status;
use jfs::Store;
use kafka::client::{SecurityConfig, KafkaClient};
use kafka::producer::{Producer, Record};
use models::MessagePayload;
use openssl::ssl::{SslContext, SslMethod};
use openssl::x509::X509FileType;
use router::Router;
use std::{path, thread};
use std::sync::{Arc, Mutex, mpsc};

fn load_kafka_client(cert_path: path::PathBuf, key_path: path::PathBuf, brokers: Vec<String>) -> KafkaClient {
    let mut context = SslContext::new(SslMethod::Tlsv1).unwrap();
    context.set_cipher_list("DEFAULT").unwrap();
    context.set_certificate_file(&cert_path, X509FileType::PEM).unwrap();
    context.set_private_key_file(&key_path, X509FileType::PEM).unwrap();

    KafkaClient::new_secure(brokers, SecurityConfig::new(context))
}

fn main() {
    let matches = utils::initialize_app().get_matches();

    let config = utils::get_args(matches);
    let copied_dry_run = config.dry_run;
    let copied_panic = config.panic_on_backup;

    let (tx, rx) = mpsc::channel();
    let original_tx = Arc::new(Mutex::new(tx));
    let new_tx = original_tx.clone();

    let db = Store::new("kafka_rust");
    if db.is_err() {
        panic!("Failed to create Backup Store!");
    }
    let db = db.unwrap();

    let kafka_client: KafkaClient;
    let producer;

    if !copied_dry_run {
        kafka_client = load_kafka_client(config.cert_path, config.key_path, config.brokers);
        producer = Some(Producer::from_client(kafka_client).create().unwrap());
    } else {
        producer = None;
    }
    let arcd_producer;
    if !copied_dry_run {
        arcd_producer = Some(Arc::new(Mutex::new(producer.unwrap())));
    } else {
        arcd_producer = None;
    }

    utils::resend_failed_messages(&db, copied_dry_run, arcd_producer.clone());

    println!("[+] Initializing Metrics Reporter.");
    let reporter = stats::Reporter{};
    println!("[+] Starting Metrics Reporter.");
    let reporter_tx = reporter.start_reporting();
    let http_reporter = reporter_tx.clone();
    let kafka_reporter = reporter_tx.clone();
    println!("[+] Done.");

    println!("[+] Initializing Failure Reporter.");
    let failure_reporter = reporter::Reporter{};
    println!("[+] Starting Failure Reporter.");
    let failed_tx = failure_reporter.start_reporting();
    println!("[+] Done.");

    let kafka_proxy = move |ref mut req: &mut Request| -> IronResult<Response> {
        let body = req.get::<bodyparser::Raw>();
        let topic = req.extensions.get::<Router>().unwrap().find("topic").unwrap();
        match body {
            Ok(Some(body)) => {
                &new_tx.lock().unwrap().send(MessagePayload {
                    topic: String::from(topic),
                    payload: body
                }).unwrap();
                if !copied_dry_run {
                    let _ = http_reporter.lock().unwrap().send(stats::Stat::new(true, true));
                }
                Ok(Response::with(status::Ok))
            },
            Ok(None) => {
                if !copied_dry_run {
                    let _ = http_reporter.lock().unwrap().send(stats::Stat::new(true, false));
                }
                Ok(Response::with(status::BadRequest))
            },
            Err(_) => {
                if !copied_dry_run {
                    let _ = http_reporter.lock().unwrap().send(stats::Stat::new(true, false));
                }
                Ok(Response::with(status::BadRequest))
            }
        }
    };

    thread::spawn(move || {
        loop {
            let possible_payload = rx.try_recv();
            if possible_payload.is_ok() {
                let message_payload = possible_payload.unwrap();
                let cloned_object = message_payload.clone();

                if copied_dry_run {
                    println!("{:?}", message_payload);
                } else {
                    let arcd_producer = arcd_producer.clone().unwrap();
                    let attempt_to_send = arcd_producer.lock().unwrap().send(&Record{
                        topic: &message_payload.topic,
                        partition: -1,
                        key: (),
                        value: message_payload.payload,
                    });

                    if attempt_to_send.is_err() {
                        let save_result = db.save(&cloned_object);
                        if save_result.is_err() {
                            if copied_panic {
                                panic!("[-] Failed to backup: [ {:?} ]", cloned_object);
                            } else {
                                println!("[-] Failed to backup: [ {:?} ]", cloned_object);
                            }
                        } else {
                            println!("[-] Failed to send: [ {:?} ] to kafka, but has been backed up.", cloned_object);
                        }

                        let _ = failed_tx.lock().unwrap().send(());
                        let _ = kafka_reporter.lock().unwrap().send(stats::Stat::new(false, false));
                    } else {
                        let _ = kafka_reporter.lock().unwrap().send(stats::Stat::new(false, true));
                    }
                }
            }
        }
    });

    let url = format!("0.0.0.0:{}", config.port);

    println!("[+] Starting Kafka Proxy at: [ {:?} ]", url);
    let router = router!(post "/kafka/:topic" => kafka_proxy);
    Iron::new(router).http(&url.as_str()).unwrap();
}
