Feature: BEP 23 — tracker compact peer list
  Reference: https://www.bittorrent.org/beps/bep_0023.html

  Scenario: parse a tracker response carrying a two-peer compact v4 list
    Given a bencoded announce response with interval 1800 and two compact v4 peers
    When the tracker response is parsed
    Then the announce interval is 1800 seconds
    And the parsed peer list contains "10.0.0.1:6881"
    And the parsed peer list contains "192.168.1.2:49205"

  Scenario: parse a tracker response carrying a single compact v6 peer
    Given a bencoded announce response with interval 900 and one compact v6 peer at "[::1]:6881"
    When the tracker response is parsed
    Then the parsed peer list contains "[::1]:6881"

  Scenario: tracker failure reason surfaces as a typed error
    Given a bencoded tracker response with failure reason "tracker down"
    When the tracker response is parsed
    Then parsing returns a tracker failure with message "tracker down"
