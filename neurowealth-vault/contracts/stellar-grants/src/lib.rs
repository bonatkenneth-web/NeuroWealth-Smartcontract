//! # StellarGrant Protocol — Smart Contracts
//!
//! This crate contains the on-chain logic for the StellarGrant grant-dispute
//! arbitration protocol built on Stellar / Soroban.
//!
//! ## Modules
//!
//! - [`arbitration_pool`] — Arbiter pool, case lifecycle, and reward
//!   settlement.

#![no_std]

pub mod arbitration_pool;

#[cfg(test)]
mod tests;
