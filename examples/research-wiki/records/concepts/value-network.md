---
type: concept
meta-type: conclusion
id: value-network
created: 2026-05-21T11:00:00Z
updated: 2026-05-23T16:45:00Z
summary: "A neural network that estimates the expected outcome from a position; TD-Gammon learned one by temporal-difference self-play, AlphaZero shares a value head with its policy"
topic: value-network
tags: [reinforcement-learning, function-approximation, value-network]
derived_from:
  - [[records/papers/tesauro-tdgammon-1995]]
  - [[records/papers/silver-alphazero-2017]]
  - [[records/papers/silver-alphago-2016]]
  - [[records/papers/mnih-dqn-2015]]
---

# Value network

A value network is a neural network that maps a state to an estimate
of its expected return — for a board game, the probability of winning
from that position.

## History

TD-Gammon ([[records/papers/tesauro-tdgammon-1995]]) learned a value
network for backgammon by [[records/concepts/temporal-difference-learning]]
updates over self-play games, the first prominent demonstration that a
learned value function could reach world-class strength. DQN
([[records/papers/mnih-dqn-2015]]) is the deep-RL descendant: its
Q-network is an action-value network mapping pixels to
[[records/concepts/q-learning]] values.

## In AlphaGo and AlphaZero

AlphaGo ([[records/papers/silver-alphago-2016]]) trained a separate value
network to predict the winner. AlphaZero
([[records/papers/silver-alphazero-2017]]) merges it into a single
network with two heads — a policy head and a value head — sharing a
common trunk. The value head guides [[records/concepts/mcts]] in place of
a random rollout. Both are deep
[[records/concepts/function-approximation]] of the position value.

## Related concepts

- [[records/concepts/self-play]]
- [[records/concepts/temporal-difference-learning]]
- [[records/concepts/function-approximation]]
- [[records/concepts/q-learning]]
- [[records/concepts/markov-decision-process]]
