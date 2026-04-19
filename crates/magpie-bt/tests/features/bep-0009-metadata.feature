Feature: BEP 9 — Extension for peers to send metadata (ut_metadata)
  Reference: https://www.bittorrent.org/beps/bep_0009.html

  Scenario: Request message round-trips through the codec
    Given a MetadataMessage::Request for piece 7
    When the wire codec encodes and decodes it
    Then the decoded message is a Request for piece 7

  Scenario: Reject message round-trips through the codec
    Given a MetadataMessage::Reject for piece 42
    When the wire codec encodes and decodes it
    Then the decoded message is a Reject for piece 42

  Scenario: Data message carries the bencoded dict followed by raw metadata bytes
    Given a MetadataMessage::Data for piece 0 with total_size 32000 and payload "raw metadata bytes here!"
    When the wire codec encodes and decodes it
    Then the decoded dict reports msg_type=1, piece=0, total_size=32000
    And the trailing bytes after the dict equal the original payload

  Scenario: Last piece may carry zero trailing bytes
    Given a MetadataMessage::Data for piece 5 with total_size 81920 and an empty payload
    When the wire codec decodes the bencoded dict alone
    Then the decoded message is Data with empty data bytes

  Scenario: Decoder rejects msg_type values other than 0, 1, 2
    Given a bencoded payload with msg_type=99
    When the wire codec decodes it
    Then decoding fails with InvalidMsgType(99)

  Scenario: Decoder rejects total_size above the 16 MB safety cap
    Given a Data payload declaring total_size 20_000_000
    When the wire codec decodes it
    Then decoding fails with TotalSizeTooLarge

  Scenario: Decoder rejects per-piece payload above 16 KiB
    Given a Data payload whose trailing bytes exceed METADATA_PIECE_SIZE (16384)
    When the wire codec decodes it
    Then decoding fails with a piece-size-limit Decode error

  Scenario: metadata_piece_count derives the 16 KiB piece count from total size
    Given a metadata size of 16383 bytes
    Then metadata_piece_count returns 1
    And for 16384 bytes it returns 1
    And for 16385 bytes it returns 2

  Scenario: Seeder advertises metadata_size in its extension handshake
    Given a torrent session constructed with known info bytes
    When the session opens an outbound peer
    Then the extension handshake we send carries metadata_size equal to the info-dict length

  Scenario: Magnet add fetches metadata, verifies SHA-1, then transitions to downloading
    Given two engines on loopback where the seeder holds full metadata + content
    And the leecher is started with magnet:?xt=urn:btih:<hash>&tr=<tracker>
    When the leecher peers with the seeder and exchanges ut_metadata pieces
    Then the leecher reassembles the info dict, verifies its SHA-1 matches the magnet info_hash
    And emits Alert::MetadataReceived
    And then proceeds to fetch all data pieces and SHA-256 of the leecher storage equals the seeder content
