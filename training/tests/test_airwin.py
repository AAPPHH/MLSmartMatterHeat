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
