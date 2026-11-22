pub(crate) mod binding;
pub(crate) mod coordinator;
pub(crate) mod display;
pub(crate) mod executor;
pub(crate) mod helper;
pub(crate) mod live;
pub(crate) mod native_discovery;
pub(crate) mod planner;
pub(crate) mod source;
pub(crate) mod types;
#[cfg(windows)]
pub(crate) mod windows;
pub(crate) mod worker;

#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod coordinator_tests;
#[cfg(test)]
mod display_tests;
#[cfg(test)]
mod executor_tests;
#[cfg(test)]
mod helper_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod native_discovery_tests;
#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod source_tests;
#[cfg(test)]
mod tests;
