# Executable ABI policy for light-controller-runtime-rkyv-v1.
RKYV_VERSION=0.8.12
REQUIRED_RKYV_FEATURES=(aligned bytecheck little_endian pointer_width_32 uuid-1)
CONFLICTING_RKYV_FEATURES=(big_endian unaligned pointer_width_16 pointer_width_64)
