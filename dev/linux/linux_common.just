set working-directory := '../..'

[group('deps')]
[linux]
deps:
    cargo xtask fetch-cef
