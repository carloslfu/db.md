---
type: wiki-page
id: value-network
created: 2026-05-21T11:00:00Z
updated: 2026-05-22T11:30:00Z
summary: "A neural network that estimates the expected outcome from a position; TD-Gammon learned one by temporal-difference self-play, AlphaZero shares a value head with its policy"
topic: value-network
tags: [reinforcement-learning, function-approximation, value-network]
derived_from:
  - [[records/papers/tesauro-tdgammon-1995]]
  - [[records/papers/silver-alphazero-2017]]
---

# Value network

A value network is a neural network that maps a state to an estimate
of its expected return — for a board game, the probability of winning
from that position.

## History

TD-Gammon ([[records/papers/tesauro-tdgammon-1995]]) learned a value
network for backgammon by temporal-difference updates over self-play
games, the first prominent demonstration that a learned value function
could reach world-class strength.

## In AlphaZero

AlphaZero ([[records/papers/silver-alphazero-2017]]) uses a single
network with two heads — a policy head and a value head — sharing a
common trunk. The value head guides [[wiki/concepts/mcts]] in place of
a random rollout.

## Related concepts

- [[wiki/concepts/self-play]]
- [[wiki/concepts/markov-decision-process]]
