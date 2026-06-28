---
type: paper
id: mnih-a3c-2016
created: 2026-04-16T15:40:00Z
updated: 2026-05-23T14:20:00Z
summary: "Mnih et al.'s A3C: asynchronous advantage actor-critic, where parallel workers explore independent environment copies, decorrelating updates without a replay buffer"
title: "Asynchronous Methods for Deep Reinforcement Learning"
authors: [Mnih, Badia, Mirza, Graves, Lillicrap, Harley, Silver, Kavukcuoglu]
year: 2016
venue: ICML
arxiv_id: "1602.01783"
url: https://arxiv.org/abs/1602.01783
tags: [reinforcement-learning, deep-rl, actor-critic, a3c, deepmind]
source: [[sources/papers/mnih-a3c-2016]]
concepts:
  - [[records/concepts/actor-critic]]
  - [[records/concepts/policy-gradient]]
  - [[records/concepts/function-approximation]]
---

# A3C (Mnih et al. 2016)

Asynchronous Advantage Actor-Critic. Instead of stabilizing learning with
a replay buffer as DQN does, A3C runs many actor-learners in parallel,
each on its own copy of the environment. The diversity of their
simultaneous experience decorrelates the updates, so a simple on-policy
actor-critic becomes stable on a single multi-core CPU. The "advantage"
is the policy-gradient signal: the actor is updated by the advantage
estimated against the critic's value baseline.

See [[records/concepts/actor-critic]] for the architecture and
[[records/concepts/policy-gradient]] for the update it scales.
