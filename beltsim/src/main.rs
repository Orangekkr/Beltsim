// beltsim CLI.
//
//   beltsim run <circuit.blt> [--seconds N | --ticks N] [--quiet-stop S]
//               [--watch S] [--probe-every S] [--probe-out FILE]
//   beltsim play <circuit.blt>
//   beltsim bench [--units K[,K2,...]] [--seconds S] [--settled]
//
// run: simulate a netlist file and print the final report plus the
//      real-time multiple achieved. --watch S redraws the display panel
//      every S sim-seconds while running.
// play: interactive REPL. step the machine, press buttons, edit smart
//      splitter rules, remap priority merger tiers, open and close gates,
//      and read the displays. This is the tic-tac-toe loop: press, run
//      until quiet (completion detection), show, repeat.
// bench: synthesize K churn (or settled) units and print a throughput table.

use beltsim::netlist::{parse_file, parse_action};
use beltsim::bench::{churn_netlist, settled_netlist};
use beltsim::netlist::parse;
use beltsim::sim::{World, TICK_RATE};
use std::io::{BufRead, Write};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage_and_exit();
    }
    match args[0].as_str() {
        "run" => cmd_run(&args[1..]),
        "play" => cmd_play(&args[1..]),
        "bench" => cmd_bench(&args[1..]),
        _ => usage_and_exit(),
    }
}

fn usage_and_exit() -> ! {
    eprintln!(
        "usage:\n  beltsim run <circuit.blt> [--seconds N | --ticks N] \
         [--quiet-stop S] [--watch S] [--shuffle SEED] [--probe-every S] [--probe-out FILE]\n  \
         beltsim play <circuit.blt>\n  \
         beltsim bench [--units K[,K2,...]] [--seconds S] [--settled]"
    );
    std::process::exit(2);
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn load(path: &str) -> World {
    match parse_file(path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("netlist error: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_run(args: &[String]) {
    let Some(path) = args.first() else {
        usage_and_exit();
    };
    let mut w = load(path);
    if let Some(s) = flag_value(args, "--shuffle") {
        let seed: u64 = s.parse().expect("bad --shuffle seed");
        w.set_shuffle(Some(seed));
    }

    let ticks: u64 = if let Some(t) = flag_value(args, "--ticks") {
        t.parse().expect("bad --ticks")
    } else {
        let secs: f64 = flag_value(args, "--seconds")
            .map(|s| s.parse().expect("bad --seconds"))
            .unwrap_or(60.0);
        (secs * TICK_RATE as f64).round() as u64
    };
    let quiet: Option<u64> = flag_value(args, "--quiet-stop")
        .map(|s| (s.parse::<f64>().expect("bad --quiet-stop") * TICK_RATE as f64) as u64);
    let watch: Option<u64> = flag_value(args, "--watch")
        .map(|s| (s.parse::<f64>().expect("bad --watch") * TICK_RATE as f64) as u64);
    if let Some(pe) = flag_value(args, "--probe-every") {
        w.probe_every =
            (pe.parse::<f64>().expect("bad --probe-every") * TICK_RATE as f64) as u64;
    } else if !w.probes.is_empty() {
        w.probe_every = TICK_RATE; // default: sample probes once per sim second
    }

    let t0 = Instant::now();
    let simulated;
    if let Some(interval) = watch {
        // Chunked run with live panel redraws.
        let mut done: u64 = 0;
        while done < ticks {
            let chunk = interval.min(ticks - done);
            w.run_ticks(chunk);
            done += chunk;
            print!("\x1b[2J\x1b[H{}", w.render_displays());
            std::io::stdout().flush().ok();
        }
        simulated = done;
        println!();
    } else {
        simulated = match quiet {
            Some(q) => w.run_until_quiet(ticks, q),
            None => {
                w.run_ticks(ticks);
                ticks
            }
        };
    }
    let wall = t0.elapsed().as_secs_f64();
    let sim_s = simulated as f64 / TICK_RATE as f64;

    print!("{}", w.report());
    println!(
        "simulated {:.3} s in {:.3} s wall: real-time multiple {:.0}x",
        sim_s,
        wall,
        if wall > 0.0 { sim_s / wall } else { f64::INFINITY }
    );

    if let Some(out) = flag_value(args, "--probe-out") {
        let mut csv = String::from("tick,sim_seconds,edge,count,head\n");
        for (tick, snap) in &w.probe_log {
            for (eid, count, head) in snap {
                csv.push_str(&format!(
                    "{},{:.4},{},{},{}\n",
                    tick,
                    *tick as f64 / TICK_RATE as f64,
                    w.edges[*eid].name,
                    count,
                    w.item_name(*head)
                ));
            }
        }
        std::fs::write(&out, csv).expect("cannot write probe csv");
        println!("probe log written to {}", out);
    }
}

const PLAY_HELP: &str = "\
commands:
  step <sec>              run <sec> simulated seconds
  quiet [max_sec]         run until settled (no state change for 2 s), or max
  press <button> [n]      queue n items on a button (default 1)
  rule <node>.<outN> <rulelist>
                          replace a smartsplitter port's rules
                          rulelist: comma-separated items, any, undefined, overflow
  priority <pm> high=<edge> [med=<edge>] [low=<edge>]
                          remap a priority merger's tiers (assign every input)
  open <gate> / close <gate>
  show                    display panel
  report                  full state report
  probe <edge>            inspect one edge
  hash                    state hash (determinism checks)
  help                    this text
  quit";

fn cmd_play(args: &[String]) {
    let Some(path) = args.first() else {
        usage_and_exit();
    };
    let mut w = load(path);
    println!(
        "beltsim interactive: {} nodes, {} edges. type help for commands.",
        w.nodes.len(),
        w.edges.len()
    );
    let panel = w.render_displays();
    if !panel.is_empty() {
        print!("{}", panel);
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("t={:.3}s> ", w.tick as f64 / TICK_RATE as f64);
        std::io::stdout().flush().ok();
        let Some(Ok(line)) = lines.next() else {
            break;
        };
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        match toks[0] {
            "quit" | "exit" | "q" => break,
            "help" | "?" => println!("{}", PLAY_HELP),
            "step" => {
                let secs: f64 = toks
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1.0);
                w.run_ticks((secs * TICK_RATE as f64).round() as u64);
                after_step(&mut w);
            }
            "quiet" => {
                let max_s: f64 = toks
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(600.0);
                let ran = w.run_until_quiet(
                    (max_s * TICK_RATE as f64).round() as u64,
                    2 * TICK_RATE,
                );
                println!(
                    "settled after {:.3} s (or hit the cap)",
                    ran as f64 / TICK_RATE as f64
                );
                after_step(&mut w);
            }
            "press" | "rule" | "priority" | "open" | "close" => {
                match parse_action(&w, &toks, 0) {
                    Ok(stim) => {
                        w.apply_stimulus(&stim);
                        println!("ok");
                    }
                    Err(e) => println!("error: {}", e),
                }
            }
            "show" => {
                let p = w.render_displays();
                if p.is_empty() {
                    println!("(no displays declared)");
                } else {
                    print!("{}", p);
                }
            }
            "report" => print!("{}", w.report()),
            "probe" => {
                if let Some(en) = toks.get(1) {
                    match w.edge_id(en) {
                        Some(e) => {
                            let edge = &w.edges[e];
                            let slots: Vec<&str> = edge
                                .slots
                                .iter()
                                .map(|&s| w.item_name(s))
                                .collect();
                            println!(
                                "{}: {}/{} head->tail [{}]{}",
                                edge.name,
                                edge.count,
                                edge.cap(),
                                slots.join(" "),
                                match (edge.has_mouth, edge.mouth) {
                                    (true, Some(m)) =>
                                        format!(" mouth: {}", w.item_name(m)),
                                    (true, None) => " mouth: empty".to_string(),
                                    _ => String::new(),
                                }
                            );
                        }
                        None => println!("unknown edge {}", en),
                    }
                } else {
                    println!("probe <edge>");
                }
            }
            "hash" => println!("{:016x}", w.state_hash()),
            other => println!("unknown command {} (try help)", other),
        }
    }
}

fn after_step(w: &mut World) {
    let p = w.render_displays();
    if !p.is_empty() {
        print!("{}", p);
    }
}

fn cmd_bench(args: &[String]) {
    let units_s = flag_value(args, "--units").unwrap_or("10,100,1000,5000".into());
    let seconds: f64 = flag_value(args, "--seconds")
        .map(|s| s.parse().expect("bad --seconds"))
        .unwrap_or(30.0);
    let settled = flag_present(args, "--settled");
    let ticks = (seconds * TICK_RATE as f64).round() as u64;

    println!(
        "beltsim Stage 0 naive oracle benchmark ({} circuits, {} sim-seconds each)",
        if settled { "settled" } else { "churn" },
        seconds
    );
    println!(
        "{:>7} {:>8} {:>8} {:>12} {:>10} {:>12}",
        "units", "nodes", "edges", "ticks", "wall_s", "multiple"
    );

    for u in units_s.split(',') {
        let k: usize = u.trim().parse().expect("bad --units");
        let text = if settled {
            settled_netlist(k)
        } else {
            churn_netlist(k)
        };
        let mut w = parse(&text).expect("bench netlist must parse");
        let nn = w.nodes.len();
        let ne = w.edges.len();
        let t0 = Instant::now();
        w.run_ticks(ticks);
        let wall = t0.elapsed().as_secs_f64();
        let mult = seconds / wall;
        println!(
            "{:>7} {:>8} {:>8} {:>12} {:>10.3} {:>11.0}x",
            k, nn, ne, ticks, wall, mult
        );
    }
    println!(
        "note: single core, deterministic, no event core yet. multiple = sim_seconds / wall_seconds."
    );
}
