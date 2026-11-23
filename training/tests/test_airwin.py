import numpy as np
from heattrain.airwin import curve, pav_decreasing, CONFIG


def test_pav_makes_a_noisy_curve_monotone_decreasing():
    out = pav_decreasing([1.0, 2.0, 0.5, 0.7, -1.0], [1, 1, 1, 1, 1])
    assert all(a >= b for a, b in zip(out, out[1:]))


def test_curve_assigns_falling_rates_positive_evidence():
    rng = np.random.default_rng(1)
    closed = rng.normal(0, 0.01, 20000)
    opened = rng.normal(-0.15, 0.03, 300)
    values = np.concatenate([closed, opened])
    flags = np.concatenate([np.zeros(len(closed), bool), np.ones(len(opened), bool)])
    c, _ = curve(CONFIG["airwin"], values, flags)
    assert c["llr"][0] > 1.0 and c["llr"][-1] <= 0.0
    assert all(a >= b for a, b in zip(c["llr"], c["llr"][1:]))


def test_decay_lambda_recovers_the_air_change_rate_of_an_exponential_decay():
    from heattrain.airwin import decay_lambda
    ms = np.arange(0, 12) * 60_000
    excess = 600.0 * np.exp(-4.4 * ms / 3_600_000.0)
    assert abs(decay_lambda(ms, excess, 4) - 4.4) < 1e-6
    assert decay_lambda(ms[:3], excess[:3], 4) is None


def test_tempering_stays_at_the_prior_scale_without_label_signal_and_moves_with_it():
    from heattrain.airwin import tempering, CONFIG
    cfg = CONFIG["airwin"]["physics"]
    n = 4000
    rng = np.random.default_rng(1)
    trace = {"a": {c: rng.normal(0, 1, n) for c in cfg["raw_columns"]}}
    none = {"a": np.zeros(n, bool)}
    w0, scale0 = tempering(cfg, trace, none, cfg["default_weight"])
    assert all(abs(s - 1.0) <= 0.8 for s in scale0)
    pos = np.zeros(n, bool)
    pos[::10] = True
    trace["a"]["r_co2"] = np.where(pos, 6.0, 0.0) + rng.normal(0, 1, n)
    w1, scale1 = tempering(cfg, trace, {"a": pos}, cfg["default_weight"])
    assert scale1[0] > scale0[0]
