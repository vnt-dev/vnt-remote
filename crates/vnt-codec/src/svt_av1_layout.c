#include <stddef.h>
#include "EbSvtAv1Enc.h"
size_t vnt_svt_layout(unsigned index) {
  const size_t values[] = {sizeof(EbSvtAv1EncConfiguration), sizeof(EbBufferHeaderType),
    sizeof(EbSvtIOFormat), offsetof(EbSvtAv1EncConfiguration, source_width),
    offsetof(EbSvtAv1EncConfiguration, target_bit_rate), offsetof(EbBufferHeaderType, pts)};
  return index < sizeof(values) / sizeof(values[0]) ? values[index] : 0;
}
