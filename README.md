# text sim

run with `cargo run > out.txt`. the sim reads from `sim.ycf`, so just edit that.
each action can either be a string with the action name, or a `[<delay> <action>]` pair.
this will delay the action from when it would normally be used by the specified amount.

the sim file uses a funny format i wrote, https://github.com/Yurihaia/ycf.

the valid jobs/action names are in the `action_lists` folder. note that job names need to be the
all uppercase abbreviation, like `SAM` or `BRD`.

for other enum names, look in https://github.com/Yurihaia/xivc/blob/master/crates/xivc-core/src/enums.rs.
the serialized names will just be the stringified version of the enum names (case matters).

for the config file data format, look in https://github.com/Yurihaia/xivc-text-sim/blob/master/src/data.rs.

good luck i guess.