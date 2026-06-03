// Re-export mediasoup-client types under stable names so the protocol module
// stays decoupled from package internals.
//
// mediasoup-client 3.20+ exposes all types from the root package — no more
// `mediasoup-client/lib/*` deep imports.

export type {
  RtpCapabilities,
  RtpParameters,
  MediaKind,
  DtlsParameters,
  IceCandidate,
  IceParameters,
  SctpParameters,
  SctpStreamParameters,
} from "mediasoup-client/types";

// Mediasoup IDs are opaque strings on the wire.
export type TransportId = string;
export type ProducerId = string;
export type ConsumerId = string;
