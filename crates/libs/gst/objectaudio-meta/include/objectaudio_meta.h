#ifndef SALSA_OBJECT_AUDIO_META_H
#define SALSA_OBJECT_AUDIO_META_H

#include <gst/gst.h>

G_BEGIN_DECLS

#define OBJECT_AUDIO_META_API_TYPE (object_audio_meta_api_get_type())
#define OBJECT_AUDIO_META_INFO (object_audio_meta_get_info())

#define OBJECT_AUDIO_PAYLOAD_OAMD 0u
#define OBJECT_AUDIO_PAYLOAD_SPATIAL 1u

typedef struct _ObjectAudioMeta ObjectAudioMeta;

struct _ObjectAudioMeta {
  GstMeta meta;
  guint32 version;
  guint32 payload_type;
  gpointer payload;
  gsize size;
  /* Version 3 mirror: max(0, payload.snapshot_offset). */
  gsize offset;
  gsize bpf;
};

GType object_audio_meta_api_get_type(void);
const GstMetaInfo *object_audio_meta_get_info(void);

/* offset must match the version 3 payload's nonnegative snapshot offset. */
ObjectAudioMeta *object_audio_meta_add(GstBuffer *buffer, gconstpointer payload,
                                       gsize size, gsize offset, gsize bpf);
guint object_audio_meta_get_n(GstBuffer *buffer);
ObjectAudioMeta *object_audio_meta_get(GstBuffer *buffer);
ObjectAudioMeta *object_audio_meta_iterate(GstBuffer *buffer, gpointer *state);

G_END_DECLS

#endif
