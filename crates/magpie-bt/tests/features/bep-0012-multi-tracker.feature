Feature: BEP 12 — multi-tracker with tier fall-through
  Reference: https://www.bittorrent.org/beps/bep_0012.html

  Scenario: announce falls through tiers on failure
    Given a TieredTracker with tier-0 containing a failing tracker
    And tier-1 containing a working tracker that returns 2 peers
    When announce is called
    Then the peer list contains 2 peers

  Scenario: successful tracker is promoted to tier head
    Given a TieredTracker with tier-0 [failing, working] in that order
    When announce is called successfully
    Then the tier-0 order becomes [working, failing]

  Scenario: all trackers failing returns the last error
    Given a TieredTracker where every tracker in every tier fails
    When announce is called
    Then the call returns the last observed TrackerError

  Scenario: shuffled tiers are deterministic under a fixed seed
    Given two TieredTracker instances built from the same tiers and seed
    When tier_order is inspected on both
    Then the orderings are identical
