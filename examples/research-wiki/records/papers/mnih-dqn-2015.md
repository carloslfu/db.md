---
type: paper
id: mnih-dqn-2015
created: 2026-04-14T13:20:00Z
updated: 2026-05-23T14:15:00Z
summary: "Mnih et al.'s DQN: a deep Q-network learning Atari from pixels at human level, stabilized by experience replay and a periodically-frozen target network"
title: "Human-level control through deep reinforcement learning"
authors: [Mnih, Kavukcuoglu, Silver, Rusu, Veness, Bellemare, Graves, Riedmiller, Fidjeland, Ostrovski, Petersen, Beattie, Sadik, Antonoglou, King, Kumaran, Wierstra, Legg, Hassabis]
year: 2015
venue: Nature
doi: "10.1038/nature14236"
url: https://www.nature.com/articles/nature14236
tags: [reinforcement-learning, deep-rl, dqn, atari, deepmind]
source: [[sources/papers/mnih-dqn-2015]]
concepts:
  - [[records/concepts/q-learning]]
  - [[records/concepts/function-approximation]]
  - [[records/concepts/experience-replay]]
  - [[records/concepts/value-network]]
---

# DQN (Mnih et al. 2015)

The deep Q-network: the result that launched deep reinforcement learning.
A single convolutional network learned to play 49 Atari 2600 games from
raw pixels and the score signal alone, reaching human-level play on the
majority. Two ingredients tamed the instability of combining Q-learning
with a nonlinear approximator: experience replay (sample minibatches from
a buffer of past transitions) and a target network frozen for a fixed
interval.

See [[records/concepts/experience-replay]] and
[[records/concepts/function-approximation]] for the stabilizers, and
[[records/concepts/q-learning]] for the underlying control rule.
