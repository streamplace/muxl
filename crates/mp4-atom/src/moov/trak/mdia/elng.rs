use crate::*;

/// ExtendedLanguageBox (ISO/IEC 14496-12 § 8.4.6).
///
/// Carries an RFC 4646 / BCP 47 language tag (e.g. `en-US`) for the media.
/// When present it takes precedence over the packed ISO-639-2/T code in
/// `mdhd`. The body is a single null-terminated UTF-8 string.
///
/// muxl vendor addition: upstream mp4-atom does not model `elng`, and
/// silently drops unknown `mdia` children, so the tag could not round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Elng {
    pub extended_language: String,
}

impl AtomExt for Elng {
    type Ext = ();

    const KIND_EXT: FourCC = FourCC::new(b"elng");

    fn decode_body_ext<B: Buf>(buf: &mut B, _ext: ()) -> Result<Self> {
        let extended_language = String::decode(buf)?;

        // Tolerate writers that pad past the terminator.
        if buf.has_remaining() {
            buf.advance(buf.remaining());
        }

        Ok(Elng { extended_language })
    }

    fn encode_body_ext<B: BufMut>(&self, buf: &mut B) -> Result<()> {
        self.extended_language.as_str().encode(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elng() {
        let expected = Elng {
            extended_language: "en-US".into(),
        };
        let mut buf = Vec::new();
        expected.encode(&mut buf).unwrap();
        assert_eq!(
            buf,
            vec![
                0x00, 0x00, 0x00, 0x12, b'e', b'l', b'n', b'g', 0x00, 0x00, 0x00, 0x00, b'e', b'n',
                b'-', b'U', b'S', 0x00
            ]
        );

        let mut buf = buf.as_ref();
        let decoded = Elng::decode(&mut buf).unwrap();
        assert_eq!(decoded, expected);
    }
}
