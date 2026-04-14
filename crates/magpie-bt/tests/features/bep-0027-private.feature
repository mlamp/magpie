Feature: BEP 27 — private torrents
  Reference: https://www.bittorrent.org/beps/bep_0027.html

  Scenario: private flag is parsed from the info dict
    Given a metainfo whose info dict carries "private": 1
    When the metainfo is parsed
    Then the parsed Info reports private = true

  Scenario: absent private key is treated as public
    Given a metainfo whose info dict omits the "private" key
    When the metainfo is parsed
    Then the parsed Info reports private = false

  Scenario: private flag is authoritative at the session layer
    Given a torrent session constructed with private = true
    When the session reports its private flag via is_private()
    Then the value is true
    And future peer-discovery subsystems (DHT / PEX / LSD) must not gossip
