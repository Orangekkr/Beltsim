# beltsim

A deterministic simulator for Satisfactory belt logic stuff.
It exists so the belt CPU can be designed, debugged, and tested at
thousands of times real speed, outside the game engine.

## Quickstart

    cargo build --release
    ./target/release/beltsim run examples/seal_flush.blt --seconds 40

You get a report: display panel, sink counts, edge levels with jam flags,
and the real time multiple.

Play commands: step <sec>, quiet [max_sec], press <button> [n],
rule <node>.<outN> <rulelist>, priority <pm> high=<edge> [med=] [low=],
open <gate>, close <gate>, show, report, probe <edge>, hash, quit.
The quiet command runs until nothing changes for two seconds, which is
completion detection: press a move, quiet, read the board, repeat.

## Writing circuits

Circuits are plain text .blt files. The smallest useful one:

    item one
    node S source item=one rate=270/min
    node K sink
    edge W S -> K.in mk=3 slots=8
    display TOTAL counter K

Full language reference, including the exact junction semantics and every
key: SYNTAX.md. 

Since v0.3 you can define reusable parts and instantiate them:

    def or2
    node M merger
    pin a M.in0
    pin b M.in1
    pin q M.out
    end

    inst g or2
    edge E1 B1 -> g.a mk=2 slots=3

Instantiation is pure inlining, so a part behaves exactly like writing its
contents by hand, tick for tick. Internals get slash names (g/M) and stay
fully addressable from probes, displays, at-statements, and the REPL.
lib/parts.blt has three small tutorial parts.

## Commands

    beltsim run <file> [--seconds N | --ticks N] [--quiet-stop S]
                [--watch S] [--probe-every S] [--probe-out FILE]
    beltsim play <file>
    beltsim bench [--units K,...] [--seconds S] [--settled]

