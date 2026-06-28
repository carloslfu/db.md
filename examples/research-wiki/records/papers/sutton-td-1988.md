---
type: paper
id: sutton-td-1988
created: 2026-04-08T10:15:00Z
updated: 2026-05-23T14:00:00Z
summary: "Sutton's foundational temporal-difference paper: TD methods learn predictions from successive estimates (bootstrapping) rather than waiting for the final outcome"
title: "Learning to Predict by the Methods of Temporal Differences"
authors: [Sutton]
year: 1988
venue: Machine Learning
doi: "10.1007/BF00115009"
url: https://link.springer.com/article/10.1007/BF00115009
tags: [reinforcement-learning, temporal-difference, prediction, foundations]
source: [[sources/papers/sutton-td-1988]]
concepts:
  - [[records/concepts/temporal-difference-learning]]
  - [[records/concepts/value-network]]
---

# Temporal differences (Sutton 1988)

The paper that named and formalized temporal-difference (TD) learning.
Sutton showed that a predictor can be updated from the difference
between two successive predictions — bootstrapping from its own later
estimate — instead of waiting for the actual outcome. TD(λ) interpolates
between one-step TD and Monte Carlo via an eligibility-trace parameter
λ. The work grounds nearly every value-based method that followed.

See [[records/concepts/temporal-difference-learning]] for the mechanism,
[[records/concepts/q-learning]] for the off-policy control case it
enabled.
