//! The tester side of the test network: title profiles, the local title check and building a report.
//!
//! Part of the public factory, see `docs/factory.md` section 9. A tester needs no model and no account,
//! only hardware and their own copy of the title. The parts are:
//!
//! * [`profile`]: the title profile file and its validation, and [`profile::check_title`].
//! * [`machine`]: the machine class of a test machine, with the GPU behind a [`machine::Probe`].
//! * [`cmd`]: the small [`cmd::CommandRunner`] trait for outside commands, and the real runner.
//! * [`verify`]: the provenance check of a build, [`verify::verify_build`].
//! * [`run`]: [`run::build_report`], which turns a run folder and a log into a
//!   [`crate::report::types::Report`]. The caller signs the report.
//!
//! * [`round`]: one round of `sommelier test`, with every outside effect behind the [`round::Effects`]
//!   trait.
//!
//! The real system sits behind `Probe`, `CommandRunner` and `Effects`, everything else is a pure
//! function.

pub mod cmd;
pub mod machine;
pub mod profile;
pub mod round;
pub mod run;
pub mod verify;

pub use cmd::{CmdOutput, CommandRunner, RunError, SystemRunner};
pub use machine::{
    CommandProbe, GpuInfo, GpuOutput, GpuSource, Probe, machine_class, machine_class_with,
};
pub use profile::{Executable, Input, Milestone, Profile, Scenario, check_title};
pub use run::{RunInput, build_report};
pub use verify::verify_build;
