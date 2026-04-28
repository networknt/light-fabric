# Light-Pingora

`light-pingora` adapts Pingora proxy services to `light-runtime`.

It is the framework layer for high-performance gateway and proxy products. The
crate keeps runtime concerns such as configuration and service lifecycle
separate from Pingora-specific proxy behavior.

## Role

- bridge Pingora services into the common runtime lifecycle
- expose transport metadata to `light-runtime`
- support gateway products without duplicating bootstrap code

## Consumers

`light-gateway` uses this framework.
