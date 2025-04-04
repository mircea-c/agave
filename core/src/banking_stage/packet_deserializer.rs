//! Deserializes packets from sigverify stage. Owned by banking stage.

use {
    super::{
        immutable_deserialized_packet::{DeserializedPacketError, ImmutableDeserializedPacket},
        packet_filter::PacketFilterFailure,
    },
    agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
    crossbeam_channel::RecvTimeoutError,
    solana_perf::packet::PacketBatch,
    solana_sdk::saturating_add_assign,
    std::time::{Duration, Instant},
};

/// Results from deserializing packet batches.
pub struct ReceivePacketResults {
    /// Deserialized packets from all received packet batches
    pub deserialized_packets: Vec<ImmutableDeserializedPacket>,
    /// Counts of packets received and errors recorded during deserialization
    /// and filtering
    pub packet_stats: PacketReceiverStats,
}

pub struct PacketDeserializer {
    /// Receiver for packet batches from sigverify stage
    packet_batch_receiver: BankingPacketReceiver,
}

#[derive(Default, Debug, PartialEq)]
pub struct PacketReceiverStats {
    /// Number of packets passing sigverify
    pub passed_sigverify_count: u64,
    /// Number of packets failing sigverify
    pub failed_sigverify_count: u64,
    /// Number of packets dropped due to sanitization error
    pub failed_sanitization_count: u64,
    /// Number of packets dropped due to prioritization error
    pub failed_prioritization_count: u64,
    /// Number of vote packets dropped
    pub invalid_vote_count: u64,
    /// Number of packets dropped due to excessive precompiles
    pub excessive_precompile_count: u64,
    /// Number of packets dropped due to insufficient compute limit
    pub insufficient_compute_limit_count: u64,
}

impl PacketReceiverStats {
    pub fn increment_error_count(&mut self, err: &DeserializedPacketError) {
        match err {
            DeserializedPacketError::ShortVecError(..)
            | DeserializedPacketError::DeserializationError(..)
            | DeserializedPacketError::SignatureOverflowed(..)
            | DeserializedPacketError::SanitizeError(..) => {
                saturating_add_assign!(self.failed_sanitization_count, 1);
            }
            DeserializedPacketError::PrioritizationFailure => {
                saturating_add_assign!(self.failed_prioritization_count, 1);
            }
            DeserializedPacketError::VoteTransactionError => {
                saturating_add_assign!(self.invalid_vote_count, 1);
            }
            DeserializedPacketError::FailedFilter(PacketFilterFailure::ExcessivePrecompiles) => {
                saturating_add_assign!(self.excessive_precompile_count, 1);
            }
            DeserializedPacketError::FailedFilter(
                PacketFilterFailure::InsufficientComputeLimit,
            ) => {
                saturating_add_assign!(self.insufficient_compute_limit_count, 1);
            }
        }
    }
}

impl PacketDeserializer {
    pub fn new(packet_batch_receiver: BankingPacketReceiver) -> Self {
        Self {
            packet_batch_receiver,
        }
    }

    /// Handles receiving packet batches from sigverify and returns a vector of deserialized packets
    pub fn receive_packets(
        &self,
        recv_timeout: Duration,
        capacity: usize,
        packet_filter: impl Fn(
            ImmutableDeserializedPacket,
        ) -> Result<ImmutableDeserializedPacket, PacketFilterFailure>,
    ) -> Result<ReceivePacketResults, RecvTimeoutError> {
        let (packet_count, packet_batches) = self.receive_until(recv_timeout, capacity)?;

        Ok(Self::deserialize_and_collect_packets(
            packet_count,
            &packet_batches,
            packet_filter,
        ))
    }

    /// Deserialize packet batches, aggregates tracer packet stats, and collect
    /// them into ReceivePacketResults
    fn deserialize_and_collect_packets(
        packet_count: usize,
        banking_batches: &[BankingPacketBatch],
        packet_filter: impl Fn(
            ImmutableDeserializedPacket,
        ) -> Result<ImmutableDeserializedPacket, PacketFilterFailure>,
    ) -> ReceivePacketResults {
        let mut packet_stats = PacketReceiverStats::default();
        let mut deserialized_packets = Vec::with_capacity(packet_count);

        for banking_batch in banking_batches {
            for packet_batch in banking_batch.iter() {
                let packet_indexes = Self::generate_packet_indexes(packet_batch);

                saturating_add_assign!(
                    packet_stats.passed_sigverify_count,
                    packet_indexes.len() as u64
                );
                saturating_add_assign!(
                    packet_stats.failed_sigverify_count,
                    packet_batch.len().saturating_sub(packet_indexes.len()) as u64
                );

                deserialized_packets.extend(Self::deserialize_packets(
                    packet_batch,
                    &packet_indexes,
                    &mut packet_stats,
                    &packet_filter,
                ));
            }
        }

        ReceivePacketResults {
            deserialized_packets,
            packet_stats,
        }
    }

    /// Receives packet batches from sigverify stage with a timeout
    fn receive_until(
        &self,
        recv_timeout: Duration,
        packet_count_upperbound: usize,
    ) -> Result<(usize, Vec<BankingPacketBatch>), RecvTimeoutError> {
        let start = Instant::now();

        let packet_batches = self.packet_batch_receiver.recv_timeout(recv_timeout)?;
        let mut num_packets_received = packet_batches
            .iter()
            .map(|batch| batch.len())
            .sum::<usize>();
        let mut messages = vec![packet_batches];

        while let Ok(packet_batches) = self.packet_batch_receiver.try_recv() {
            trace!("got more packet batches in packet deserializer");
            num_packets_received += packet_batches
                .iter()
                .map(|batch| batch.len())
                .sum::<usize>();
            messages.push(packet_batches);

            if start.elapsed() >= recv_timeout || num_packets_received >= packet_count_upperbound {
                break;
            }
        }

        Ok((num_packets_received, messages))
    }

    fn generate_packet_indexes(packet_batch: &PacketBatch) -> Vec<usize> {
        packet_batch
            .iter()
            .enumerate()
            .filter(|(_, pkt)| !pkt.meta().discard())
            .map(|(index, _)| index)
            .collect()
    }

    fn deserialize_packets<'a>(
        packet_batch: &'a PacketBatch,
        packet_indexes: &'a [usize],
        packet_stats: &'a mut PacketReceiverStats,
        packet_filter: &'a impl Fn(
            ImmutableDeserializedPacket,
        ) -> Result<ImmutableDeserializedPacket, PacketFilterFailure>,
    ) -> impl Iterator<Item = ImmutableDeserializedPacket> + 'a {
        packet_indexes.iter().filter_map(move |packet_index| {
            let packet_clone = packet_batch[*packet_index].clone();

            match ImmutableDeserializedPacket::new(&packet_clone)
                .and_then(|packet| packet_filter(packet).map_err(Into::into))
            {
                Ok(packet) => Some(packet),
                Err(err) => {
                    packet_stats.increment_error_count(&err);
                    None
                }
            }
        })
    }

    #[allow(dead_code)]
    pub(crate) fn deserialize_packets_with_indexes(
        packet_batch: &PacketBatch,
    ) -> impl Iterator<Item = (ImmutableDeserializedPacket, usize)> + '_ {
        let packet_indexes = PacketDeserializer::generate_packet_indexes(packet_batch);
        packet_indexes.into_iter().filter_map(move |packet_index| {
            let packet = packet_batch[packet_index].clone();
            ImmutableDeserializedPacket::new(&packet)
                .ok()
                .map(|packet| (packet, packet_index))
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_perf::packet::to_packet_batches,
        solana_sdk::{
            hash::Hash, pubkey::Pubkey, signature::Keypair, system_transaction,
            transaction::Transaction,
        },
    };

    fn random_transfer() -> Transaction {
        system_transaction::transfer(&Keypair::new(), &Pubkey::new_unique(), 1, Hash::default())
    }

    #[test]
    fn test_deserialize_and_collect_packets_empty() {
        let results = PacketDeserializer::deserialize_and_collect_packets(0, &[], Ok);
        assert_eq!(results.deserialized_packets.len(), 0);
        assert_eq!(results.packet_stats.passed_sigverify_count, 0);
        assert_eq!(results.packet_stats.failed_sigverify_count, 0);
    }

    #[test]
    fn test_deserialize_and_collect_packets_simple_batches() {
        let transactions = vec![random_transfer(), random_transfer()];
        let packet_batches = to_packet_batches(&transactions, 1);
        assert_eq!(packet_batches.len(), 2);

        let packet_count: usize = packet_batches.iter().map(|x| x.len()).sum();
        let results = PacketDeserializer::deserialize_and_collect_packets(
            packet_count,
            &[BankingPacketBatch::new(packet_batches)],
            Ok,
        );
        assert_eq!(results.deserialized_packets.len(), 2);
        assert_eq!(results.packet_stats.passed_sigverify_count, 2);
        assert_eq!(results.packet_stats.failed_sigverify_count, 0);
    }

    #[test]
    fn test_deserialize_and_collect_packets_simple_batches_with_failure() {
        let transactions = vec![random_transfer(), random_transfer()];
        let mut packet_batches = to_packet_batches(&transactions, 1);
        assert_eq!(packet_batches.len(), 2);
        packet_batches[0][0].meta_mut().set_discard(true);

        let packet_count: usize = packet_batches.iter().map(|x| x.len()).sum();
        let results = PacketDeserializer::deserialize_and_collect_packets(
            packet_count,
            &[BankingPacketBatch::new(packet_batches)],
            Ok,
        );
        assert_eq!(results.deserialized_packets.len(), 1);
        assert_eq!(results.packet_stats.passed_sigverify_count, 1);
        assert_eq!(results.packet_stats.failed_sigverify_count, 1);
    }
}
