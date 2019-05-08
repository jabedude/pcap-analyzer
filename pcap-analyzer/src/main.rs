#[macro_use]
extern crate log;

extern crate clap;
use clap::{Arg,App,crate_version};

extern crate env_logger;
extern crate flate2;
extern crate xz2;

use std::fs::File;
use std::io;
use std::path::Path;

use flate2::read::GzDecoder;
use xz2::read::XzDecoder;

use libpcap_analyzer::{plugins,Analyzer,Config};
use libpcap_tools::PcapEngine;

fn load_config(config:&mut Config, filename:&str) -> Result<(),io::Error> {
    debug!("Loading configuration {}", filename);
    let path = Path::new(&filename);
    let file = File::open(path)?;
    config.load_config(file)
}

fn main() -> io::Result<()> {
   let matches = App::new("Pcap analyzer")
        .version(crate_version!())
        .author("Pierre Chifflier")
        .about("Tool for Pcap file analyzis")
        .arg(Arg::with_name("verbose")
             .help("Be verbose")
             .short("v")
             .long("verbose"))
        .arg(Arg::with_name("plugins")
             .help("Plugins to load (default: all)")
             .short("p")
             .long("plugins")
             .takes_value(true))
        .arg(Arg::with_name("config")
             .help("Configuration file")
             .short("c")
             .long("config")
             .takes_value(true))
        .arg(Arg::with_name("INPUT")
             .help("Input file name")
             .required(true)
             .index(1))
        .get_matches();

   env_logger::init();

   debug!("Pcap analyser {}", crate_version!());

   let builder = plugins::plugins_factory();
   let mut config = Config::default();
   if let Some(filename) = matches.value_of("config") {
       load_config(&mut config, filename)?;
   }
   let mut plugins = plugins::build_plugins(&builder,&config);
   if let Some(plugin_names) = matches.value_of("plugins") {
       debug!("Restricting plugins to: {}", plugin_names);
       let names : Vec<_> = plugin_names.split(",").collect();
       plugins.storage.retain(|k, _| {
           names.iter().any(|&x| k.contains(x))
       });
   }

   debug!("  Plugins loaded: {}", plugins.storage.len());
   debug!("  Plugins: {}", plugins.storage.keys().map(|s| s.as_ref()).collect::<Vec<_>>().join(", "));
   if plugins.storage.is_empty() {
       warn!("No plugins loaded");
   }
   let analyzer = Analyzer::new(plugins);
   let mut engine = PcapEngine::new(Box::new(analyzer));

   let input_filename = matches.value_of("INPUT").unwrap();
   // let verbose = matches.is_present("verbose");

   let mut input_reader =
       if input_filename == "-" {
           Box::new(io::stdin()) as Box<io::Read>
       } else {
           let path = Path::new(&input_filename);
           let file = File::open(path)?;
           if input_filename.ends_with(".gz") {
               Box::new(GzDecoder::new(file)) as Box<io::Read>
           } else if input_filename.ends_with(".xz") {
               Box::new(XzDecoder::new(file)) as Box<io::Read>
           } else {
               Box::new(file)
           }
       };

   let _ = engine.run(&mut input_reader).expect("run analyzer");

   Ok(())
}
