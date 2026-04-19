Feature: BEP 14 — Local Service Discovery
  Reference: https://www.bittorrent.org/beps/bep_0014.html

  Scenario: LsdAnnounce round-trips a single info hash with cookie
    Given an LsdAnnounce for port 6881 with one info hash and cookie "test-cookie"
    When the announce is encoded then decoded
    Then the decoded announce equals the original

  Scenario: LsdAnnounce round-trips multiple info hashes
    Given an LsdAnnounce carrying two info hashes
    When the announce is encoded then decoded
    Then the decoded message preserves both hashes in order

  Scenario: Decoder accepts the canonical CRLF wire format
    Given the wire bytes "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: <40-hex>\r\ncookie: abc123\r\n\r\n"
    When the LsdAnnounce decoder processes the bytes
    Then the announce reports port 6881, the parsed info hash, and cookie "abc123"

  Scenario: Decoder is lenient about LF-only and mixed line endings
    Given the same announce serialised with bare LF separators
    When the LsdAnnounce decoder processes the bytes
    Then it parses successfully

  Scenario: Decoder rejects messages missing the Port header
    Given a wire message with Infohash but no Port header
    When the LsdAnnounce decoder processes the bytes
    Then decoding fails with MissingPort

  Scenario: Decoder rejects messages missing any Infohash header
    Given a wire message with Port but no Infohash header
    When the LsdAnnounce decoder processes the bytes
    Then decoding fails with MissingInfoHash

  Scenario: Decoder rejects malformed info hashes
    Given a wire message whose Infohash value is not 40 lowercase-hex chars
    When the LsdAnnounce decoder processes the bytes
    Then decoding fails with InvalidInfoHash

  Scenario: Decoder rejects messages whose request line is not BT-SEARCH
    Given a wire message starting with "GET / HTTP/1.1"
    When the LsdAnnounce decoder processes the bytes
    Then decoding fails with InvalidFormat

  Scenario: Decoder caps the number of info hashes per message
    Given a wire message containing more than 100 Infohash headers
    When the LsdAnnounce decoder processes the bytes
    Then decoding fails with InvalidFormat ("too many info hashes")

  Scenario: LsdService binds to INADDR_ANY with SO_REUSEADDR so multiple listeners coexist
    Given two LsdService bind attempts on the same multicast port
    When both services are bound concurrently
    Then both bind calls succeed

  Scenario: LsdService's outbound announce uses TTL=1 and the configured multicast group
    Given a default LsdConfig
    Then the bound socket has multicast TTL = 1
    And announces are sent to 239.192.152.143:6771

  Scenario: LsdService filters its own announcements via the cookie field
    Given an LsdService that emitted an announce with cookie C
    When the same service receives a multicast packet whose cookie equals C
    Then no LsdDiscovery is forwarded to the consumer

  Scenario: Private torrents are never registered for LSD announce
    Given an LsdService handle and a private info hash
    When register(hash, private=true) is called
    Then no register command reaches the background task and no announce is scheduled

  Scenario: Two engines on loopback discover each other via multicast
    Given engine A registers info hash H and engine B is listening on the same loopback multicast group
    When A emits its periodic announce
    Then B forwards an LsdDiscovery for H carrying A's announced port
