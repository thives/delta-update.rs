//! Shared, std-only clap arguments for the host tools; the payload library stays clap-free.

use clap::Args;
use ota_payload::profile::{EMBEDDED, ResourceProfile};

/// Parse a host command, including usage on every argument error (but not help/version).
pub fn parse<T: clap::Parser>() -> T {
    T::try_parse().unwrap_or_else(|mut error| {
        if error.use_stderr() {
            error.insert(
                clap::error::ContextKind::Usage,
                clap::error::ContextValue::StyledStr(T::command().render_usage()),
            );
        }
        error.exit()
    })
}

/// Explicit resource caps. Defaults follow the library's embedded baseline, not a profile selector.
#[derive(Args, Debug)]
#[group(skip)]
pub struct ResourceArgs {
    /// Maximum encoded manifest size in bytes
    #[arg(long, default_value_t = EMBEDDED.max_encoded_manifest, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_encoded_manifest: usize,
    /// Maximum encoded metadata or payload signature message size in bytes
    #[arg(long, default_value_t = EMBEDDED.max_signature_message, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_signature_message: usize,
    /// Maximum modeled decode peak of one message in bytes (not total process memory)
    #[arg(long, default_value_t = EMBEDDED.max_decoded_bytes, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_decoded_bytes: usize,
    /// Maximum operation count across the manifest; zero permits only no-op updates
    #[arg(long, default_value_t = EMBEDDED.max_operations, value_name = "COUNT", help_heading = "Resource limits")]
    pub max_operations: usize,
    /// Maximum destination plus source extent count in one operation
    #[arg(long, default_value_t = EMBEDDED.max_extents_per_op, value_name = "COUNT", help_heading = "Resource limits")]
    pub max_extents_per_op: usize,
    /// Maximum extent count across all operations
    #[arg(long, default_value_t = EMBEDDED.max_total_extents, value_name = "COUNT", help_heading = "Resource limits")]
    pub max_total_extents: usize,
    /// Maximum partition count in the manifest
    #[arg(long, default_value_t = EMBEDDED.max_partitions, value_name = "COUNT", help_heading = "Resource limits")]
    pub max_partitions: usize,
    /// Maximum signature entry count in one signature message
    #[arg(long, default_value_t = EMBEDDED.max_signatures, value_name = "COUNT", help_heading = "Resource limits")]
    pub max_signatures: usize,
    /// Maximum size of one signature's data in bytes
    #[arg(long, default_value_t = EMBEDDED.max_signature_data, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_signature_data: usize,
    /// Maximum accepted codec block size in bytes (1..=4294967295)
    #[arg(long, default_value_t = EMBEDDED.max_block_size, value_parser = clap::value_parser!(u32).range(1..), value_name = "BYTES", help_heading = "Resource limits")]
    pub max_block_size: u32,
    /// Maximum packed frame table size per framed or sparse operation in bytes
    #[arg(long, default_value_t = EMBEDDED.max_frame_table_bytes, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_frame_table_bytes: usize,
    /// Maximum packed sparse block map size in bytes (four blocks per byte)
    #[arg(long, default_value_t = EMBEDDED.max_map_bytes, value_name = "BYTES", help_heading = "Resource limits")]
    pub max_map_bytes: usize,
}

impl From<ResourceArgs> for ResourceProfile {
    fn from(args: ResourceArgs) -> Self {
        Self {
            max_encoded_manifest: args.max_encoded_manifest,
            max_signature_message: args.max_signature_message,
            max_decoded_bytes: args.max_decoded_bytes,
            max_operations: args.max_operations,
            max_extents_per_op: args.max_extents_per_op,
            max_total_extents: args.max_total_extents,
            max_partitions: args.max_partitions,
            max_signatures: args.max_signatures,
            max_signature_data: args.max_signature_data,
            max_block_size: args.max_block_size,
            max_frame_table_bytes: args.max_frame_table_bytes,
            max_map_bytes: args.max_map_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct TestArgs {
        #[command(flatten)]
        resources: ResourceArgs,
    }

    #[test]
    fn defaults_match_embedded() {
        TestArgs::command().debug_assert();
        let args = TestArgs::try_parse_from(["test"]).unwrap();
        assert_eq!(ResourceProfile::from(args.resources), EMBEDDED);
    }

    #[test]
    fn every_limit_is_explicit_and_independent() {
        let args = TestArgs::try_parse_from([
            "test",
            "--max-encoded-manifest",
            "1",
            "--max-signature-message",
            "2",
            "--max-decoded-bytes",
            "3",
            "--max-operations",
            "0",
            "--max-extents-per-op",
            "5",
            "--max-total-extents",
            "6",
            "--max-partitions",
            "7",
            "--max-signatures",
            "8",
            "--max-signature-data",
            "9",
            "--max-block-size",
            "10",
            "--max-frame-table-bytes",
            "11",
            "--max-map-bytes",
            "12",
        ])
        .unwrap();
        assert_eq!(
            ResourceProfile::from(args.resources),
            ResourceProfile {
                max_encoded_manifest: 1,
                max_signature_message: 2,
                max_decoded_bytes: 3,
                max_operations: 0,
                max_extents_per_op: 5,
                max_total_extents: 6,
                max_partitions: 7,
                max_signatures: 8,
                max_signature_data: 9,
                max_block_size: 10,
                max_frame_table_bytes: 11,
                max_map_bytes: 12,
            }
        );
    }

    #[test]
    fn caps_accept_zero_and_native_maximum_but_reject_invalid_integers() {
        let command = TestArgs::command();
        for arg in command.get_arguments().filter(|a| a.get_long().is_some()) {
            let flag = format!("--{}", arg.get_long().unwrap());
            let is_block = flag == "--max-block-size";
            if !is_block {
                TestArgs::try_parse_from(["test", &flag, "0"]).unwrap();
            }
            let max = if is_block {
                u32::MAX.to_string()
            } else {
                usize::MAX.to_string()
            };
            TestArgs::try_parse_from(["test", &flag, &max]).unwrap();
            for invalid in ["-1", "1.5", "NaN", "18446744073709551616"] {
                assert!(
                    TestArgs::try_parse_from(["test", &flag, invalid]).is_err(),
                    "{flag} {invalid}"
                );
            }
        }
        assert!(TestArgs::try_parse_from(["test", "--max-block-size", "0"]).is_err());
        assert!(TestArgs::try_parse_from(["test", "--max-block-size", "4294967296"]).is_err());
    }
}
