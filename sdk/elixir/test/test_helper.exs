# SPDX-License-Identifier: Apache-2.0
# `@tag :live` tests talk to a real `zygo api` and run only when one is named:
#   ZYGO_LIVE=1 ZYGO_API_URL=unix:///run/user/1000/zygo/api.sock mix test --only live
exclude = if System.get_env("ZYGO_LIVE"), do: [], else: [:live]
ExUnit.start(exclude: exclude)
