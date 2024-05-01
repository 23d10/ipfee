use bincode::Options;
use clap::{App, Arg};
use crossbeam::channel::{unbounded, RecvTimeoutError};
use lru::LruCache;
use solana_sdk::ipfee::IpFeeMsg;
use solana_sdk::signature::Signature;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::num::NonZeroUsize;
use std::sync::Arc;

const PRINT_STATS_INTERVAL: u64 = 1000 * 10; // 10 seconds
const TX_COUNT_HALVING_INTERVAL: u64 = 1000 * 30; // 30 seconds;
const CREATE_IP_BLOCKLIST_INTERVAL: u64 = 1000 * 30; // 30 seconds;

// const TX_COUNT_HALVING_INTERVAL: u64 = 1000 * 60 * 60 * 6; // 6 hours;

struct State {
    // Map from tx to the IpAddr that submitted it
    ip_lookup: LruCache<Signature, IpAddr>,
    ip_avg_fees: LruCache<IpAddr, (u64, u64)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            ip_lookup: LruCache::new(NonZeroUsize::new(100_000).unwrap()),
            ip_avg_fees: LruCache::new(NonZeroUsize::new(100_000).unwrap()),
        }
    }
}

impl State {
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self { ip_lookup: LruCache::new(capacity), ip_avg_fees: LruCache::new(capacity) }
    }

    pub fn usertx(
        &mut self,
        ip: IpAddr,
        signature: Signature,
    ) {
        self.ip_lookup.put(signature, ip);
    }

    pub fn fee(
        &mut self,
        signature: Signature,
        fee: u64,
    ) {
        if let Some(ip) = self.ip_lookup.get(&signature) {
            let entry = self.ip_avg_fees.get_or_insert_mut(*ip, || (0, 0));

            let new_count = entry.0 + 1;
            // Calculate the new average fee for this IP, rounding
            // to the nearest whole number.
            entry.1 = (entry.0 * entry.1 + fee) / new_count;
            entry.0 = new_count;
        }
    }

    pub fn tx_count_havling(&mut self) {
        // Iterate over each key in the cache
        let keys: Vec<IpAddr> = self.ip_avg_fees.iter().map(|(ip, _)| *ip).collect();

        println!("Halving tx counts");
        for key in keys {
            if let Some((first, _second)) = self.ip_avg_fees.get_mut(&key) {
                *first /= 2; // Halve the tx count
            }
        }
    }

    pub fn create_ip_blocklist(&self) {
        // Step 1: Extract and sort all records by "first" descending
        let mut all_records: Vec<(IpAddr, (u64, u64))> =
            self.ip_avg_fees.iter().map(|(&ip, &fees)| (ip, fees)).collect();
        all_records.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));

        // Step 2: Get the first 100 items and find the 25th percentile of the "second" value
        let top_100: Vec<&(u64, u64)> = all_records.iter().map(|item| &item.1).take(100).collect();
        let p25_index = top_100.len() / 4; // Calculate the index for the 25th percentile
        let sorted_by_second: Vec<&(u64, u64)> = {
            let mut temp = top_100.clone();
            temp.sort_by(|a, b| a.1.cmp(&b.1));
            temp
        };
        let minimum_fee = sorted_by_second[p25_index].1;

        // Step 3: Fetch the list of IPs in the top 500 where "second" is below "minimum fee"
        let qualifying_ips: Vec<IpAddr> =
            all_records.iter().filter(|&(_, fees)| fees.1 < minimum_fee).take(500).map(|(ip, _)| *ip).collect();

        // TODO: Add a filter to only block an IP address if it's sent more than 50 txs?

        // Print the qualifying IPs
        println!("Qualifying IPs with second value below minimum fee ({}): {:?}", minimum_fee, qualifying_ips);
    }

    pub fn print_ip_stats(&self) {
        let mut outputs: Vec<(u64, String)> = Vec::new();
        let mut total_txs: u64 = 0;
        let mut avg_fees: u64 = 0;

        for (ip, fees) in self.ip_avg_fees.iter() {
            outputs.push((fees.0, format!("{}\t{}\t{}", ip, fees.0, fees.1)));

            if total_txs + fees.0 != 0 {
                avg_fees = (total_txs * avg_fees + fees.0 * fees.1) / (total_txs + fees.0);
            }
            total_txs += fees.0;
        }

        outputs.sort_by(|a, b| b.0.cmp(&a.0)); // Sort by tx count desc

        println!("TotalTxs: {}, AvgFees: {}", total_txs, avg_fees);
        println!("IP\t\tTxCount\tAvgFees");
        for (_, output) in outputs {
            println!("{}", output);
        }
        println!("");
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn main() {
    let matches = App::new("ipfee")
        .arg(Arg::with_name("address").help("The IP address to listen on").required(true).index(1))
        .arg(Arg::with_name("port").help("The port to listen on").required(true).index(2))
        .get_matches();

    let addr = matches.value_of("address").unwrap().parse::<Ipv4Addr>().unwrap_or_else(|e| {
        eprintln!("ERROR: Invalid listen address: {e}");
        std::process::exit(-1);
    });

    let port = matches.value_of("port").unwrap().parse::<u16>().unwrap_or_else(|e| {
        eprintln!("ERROR: Invalid listen port: {e}");
        std::process::exit(-1);
    });

    // Listen
    let tcp_listener = loop {
        match TcpListener::bind(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(addr, port))) {
            Ok(tcp_listener) => break tcp_listener,
            Err(e) => {
                eprintln!("Failed bind because {e}, trying again in 1 second");
                std::thread::sleep(std::time::Duration::from_secs(1));
            },
        }
    };

    let (sender, receiver) = unbounded::<IpFeeMsg>();

    let sender = Arc::new(sender);

    // Spawn the listener
    std::thread::spawn(move || {
        loop {
            let mut tcp_stream = loop {
                match tcp_listener.accept() {
                    Ok((tcp_stream, _)) => break tcp_stream,
                    Err(e) => eprintln!("Failed accept because {e}"),
                }
            };

            {
                let sender = sender.clone();

                // Spawn a thread to handle this TCP stream.  Multiple streams are accepted at once, to allow e.g.
                // a JITO relayer and a validator to both connect.
                std::thread::spawn(move || {
                    let options = bincode::DefaultOptions::new();

                    loop {
                        match options.deserialize_from::<_, IpFeeMsg>(&mut tcp_stream) {
                            Ok(tx_ingest_msg) => sender.send(tx_ingest_msg).expect("crossbeam failed"),
                            Err(e) => {
                                eprintln!("Failed deserialize because {e}; closing connection");
                                tcp_stream.shutdown(std::net::Shutdown::Both).ok();
                                break;
                            },
                        }
                    }
                });
            }
        }
    });

    let mut state = State::new(NonZeroUsize::new(100_000).unwrap());
    let mut last_log_timestamp = 0;
    let mut last_tx_count_halving_timestamp: u64 = 0;
    let mut last_create_ip_blocklist_timestamp: u64 = 0;

    loop {
        // Receive with a timeout
        match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => (),
            Ok(IpFeeMsg::UserTx { ip, signature }) => state.usertx(ip, signature),
            Ok(IpFeeMsg::Fee { signature, fee }) => state.fee(signature, fee),
        }

        let now = now_millis();

        // Check if it's time to print stats
        if now >= (last_log_timestamp + PRINT_STATS_INTERVAL) {
            state.print_ip_stats();
            last_log_timestamp = now;
        }

        // Check if it's time to halve the transaction count
        if now >= (last_tx_count_halving_timestamp + TX_COUNT_HALVING_INTERVAL) {
            state.tx_count_havling();
            last_tx_count_halving_timestamp = now;
        }

        // Check if it's time to create ip blocklist
        if now >= (last_create_ip_blocklist_timestamp + CREATE_IP_BLOCKLIST_INTERVAL) {
            state.create_ip_blocklist();
            last_create_ip_blocklist_timestamp = now;
        }
    }
}
