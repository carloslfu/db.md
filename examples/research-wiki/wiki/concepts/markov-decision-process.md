---
type: wiki-page
id: markov-decision-process
created: 2026-05-19T08:00:00Z
updated: 2026-05-19T08:00:00Z
summary: "The formal model underneath reinforcement learning: states, actions, transition probabilities, rewards, and a discount factor, with the Markov property"
topic: markov-decision-process
tags: [reinforcement-learning, theory, foundations, markov-decision-process]
---

# Markov decision process

A Markov decision process (MDP) is the formal model that reinforcement
learning optimizes over. It is the tuple (S, A, P, R, gamma): a set of
states S, a set of actions A, transition probabilities P(s' | s, a), a
reward function R(s, a), and a discount factor gamma in [0, 1).

## The Markov property

The defining assumption is that the future depends on the present
state alone, not on the path taken to reach it. Everything relevant to
the next transition is captured in the current state.

## Why it is foundational

Value functions, policies, and the Bellman equations are all defined
relative to an MDP. The methods elsewhere in this wiki —
[[wiki/concepts/policy-iteration]], [[wiki/concepts/value-network]],
and the search in [[wiki/concepts/mcts]] — are techniques for solving
or approximating solutions to an MDP, usually when P and R are unknown
or the state space is too large to enumerate.

This page is hand-curated foundational theory and is frozen (see
`DB.md` `## Policies`); the agent does not auto-edit it.
