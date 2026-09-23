/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_custom_size_t()">\
/* size_t */

static inline size_t
vn_sizeof_size_t(const size_t *val)
{
    return vn_sizeof_uint64_t(&(uint64_t){ *val });
}

static inline void
vn_encode_size_t(struct vn_cs_encoder *enc, const size_t *val)
{
    const uint64_t tmp = *val;
    vn_encode_uint64_t(enc, &tmp);
}

static inline void
vn_decode_size_t(struct vn_cs_decoder *dec, size_t *val)
{
    uint64_t tmp;
    vn_decode_uint64_t(dec, &tmp);
    *val = tmp;
}

static inline size_t
vn_sizeof_size_t_array(const size_t *val, uint32_t count)
{
    return vn_sizeof_size_t(val) * count;
}

static inline void
vn_encode_size_t_array(struct vn_cs_encoder *enc, const size_t *val, uint32_t count)
{
    if (sizeof(size_t) == sizeof(uint64_t)) {
        vn_encode_uint64_t_array(enc, (const uint64_t *)val, count);
    } else {
        for (uint32_t i = 0; i < count; i++)
            vn_encode_size_t(enc, &val[i]);
    }
}

static inline void
vn_decode_size_t_array(struct vn_cs_decoder *dec, size_t *val, uint32_t count)
{
    if (sizeof(size_t) == sizeof(uint64_t)) {
        vn_decode_uint64_t_array(dec, (uint64_t *)val, count);
    } else {
        for (uint32_t i = 0; i < count; i++)
            vn_decode_size_t(dec, &val[i]);
    }
}
</%def>

<%def name="vn_custom_blob()">\
/* opaque blob */

static inline size_t
vn_sizeof_blob_array(const void *val, size_t size)
{
    return (size + 3) & ~3;
}

static inline void
vn_encode_blob_array(struct vn_cs_encoder *enc, const void *val, size_t size)
{
    vn_encode(enc, (size + 3) & ~3, val, size);
}

static inline void
vn_decode_blob_array(struct vn_cs_decoder *dec, void *val, size_t size)
{
    vn_decode(dec, (size + 3) & ~3, val, size);
}
</%def>

<%def name="vn_custom_string()">\
/* string */

static inline size_t
vn_sizeof_char_array(const char *val, size_t size)
{
    return vn_sizeof_blob_array(val, size);
}

static inline void
vn_encode_char_array(struct vn_cs_encoder *enc, const char *val, size_t size)
{
    assert(size && strlen(val) < size);
    vn_encode_blob_array(enc, val, size);
}

static inline void
vn_decode_char_array(struct vn_cs_decoder *dec, char *val, size_t size)
{
    vn_decode_blob_array(dec, val, size);
    if (size)
        val[size - 1] = '\0';
    else
        vn_cs_decoder_set_fatal(dec);
}
</%def>

<%def name="vn_custom_array_size()">\
/* array size (uint64_t) */

static inline size_t
vn_sizeof_array_size(uint64_t size)
{
    return vn_sizeof_uint64_t(&size);
}

static inline void
vn_encode_array_size(struct vn_cs_encoder *enc, uint64_t size)
{
    vn_encode_uint64_t(enc, &size);
}

static inline uint64_t
vn_decode_array_size(struct vn_cs_decoder *dec, uint64_t expected_size)
{
    uint64_t size;
    vn_decode_uint64_t(dec, &size);
    if (size != expected_size) {
        vn_cs_decoder_set_fatal(dec);
        size = 0;
    }
    return size;
}

static inline uint64_t
vn_decode_array_size_unchecked(struct vn_cs_decoder *dec)
{
    uint64_t size;
    vn_decode_uint64_t(dec, &size);
    return size;
}

static inline uint64_t
vn_peek_array_size(struct vn_cs_decoder *dec)
{
    uint64_t size;
    vn_cs_decoder_peek(dec, sizeof(size), &size, sizeof(size));
    return size;
}

/* non-array pointer */

static inline size_t
vn_sizeof_simple_pointer(const void *val)
{
    return vn_sizeof_array_size(val ? 1 : 0);
}

static inline bool
vn_encode_simple_pointer(struct vn_cs_encoder *enc, const void *val)
{
    vn_encode_array_size(enc, val ? 1 : 0);
    return val;
}

static inline bool
vn_decode_simple_pointer(struct vn_cs_decoder *dec)
{
    return vn_decode_array_size_unchecked(dec);
}
</%def>
