// Wire protocol for `dun-tunel/poc/neko-sfu/src/bin/sfu_main.rs`.
//
// Discriminator field is `action`. JSON body uses camelCase per server-side
// `#[serde(rename_all = "camelCase")]`.

import type {
  ConsumerId,
  DtlsParameters,
  IceCandidate,
  IceParameters,
  MediaKind,
  ProducerId,
  RtpCapabilities,
  RtpParameters,
  SctpParameters,
  SctpStreamParameters,
  TransportId,
} from "./mediasoup-types";

export interface TransportOptions {
  id: TransportId;
  dtlsParameters: DtlsParameters;
  iceCandidates: IceCandidate[];
  iceParameters: IceParameters;
}

export type ServerMessage =
  | {
      action: "Init";
      consumerTransportOptions: TransportOptions;
      inputTransportOptions: TransportOptions;
      routerRtpCapabilities: RtpCapabilities;
      plainProducerId: ProducerId | null;
      sctpParameters: SctpParameters | null;
      inputSctpParameters: SctpParameters | null;
    }
  | { action: "ConnectedConsumerTransport" }
  | { action: "ConnectedInputTransport" }
  | {
      action: "Consumed";
      id: ConsumerId;
      producerId: ProducerId;
      kind: MediaKind;
      rtpParameters: RtpParameters;
    }
  | {
      action: "InputAck";
      sequence: number;
      receivedBytes: number;
    };

export type ClientMessage =
  | { action: "Init"; rtpCapabilities: RtpCapabilities }
  | { action: "ConnectConsumerTransport"; dtlsParameters: DtlsParameters }
  | { action: "ConnectInputTransport"; dtlsParameters: DtlsParameters }
  | { action: "Consume"; producerId: ProducerId }
  | { action: "ConsumerResume"; id: ConsumerId }
  | {
      action: "ProduceInput";
      sctpStreamParameters: SctpStreamParameters;
      label: string;
      protocol: string;
    };

export interface PlainProducerInfo {
  producerId: ProducerId;
  rtpListenIp: string;
  rtpListenPort: number;
  rtcpListenPort: number;
  payloadType: number;
  clockRate: number;
  encodingName: string;
  ssrc: number;
}
