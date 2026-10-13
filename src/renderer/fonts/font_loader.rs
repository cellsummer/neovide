use std::{
    collections::HashSet,
    fmt::{Display, Formatter},
    num::NonZeroUsize,
    rc::Rc,
};

use log::{trace, warn};
use lru::LruCache;
use skia_safe::{
    Data, Font, FontArguments, FontHinting as SkiaHinting, FontMgr, FourByteTag, Typeface,
    font::Edging as SkiaEdging,
    font_arguments::{VariationPosition, variation_position::Coordinate},
};

use crate::{
    profiling::tracy_zone,
    renderer::fonts::{
        font_options::{CoarseStyle, FontDescription, FontEdging, FontHinting, FontVariation},
        swash_font::SwashFont,
    },
};

static DEFAULT_FONT: &[u8] = include_bytes!("../../../assets/fonts/FiraCodeNerdFont-Regular.ttf");
static LAST_RESORT_FONT: &[u8] = include_bytes!("../../../assets/fonts/LastResort-Regular.ttf");

pub struct FontPair {
    pub key: FontKey,
    pub skia_font: Font,
    pub swash_font: SwashFont,
}

impl FontPair {
    fn new(key: FontKey, skia_font: Font) -> Option<FontPair> {
        let (font_data, index) = font_data_and_index(&skia_font.typeface())?;
        let swash_font = SwashFont::from_data(font_data, index)?;

        Some(Self::from_parts(key, skia_font, swash_font))
    }

    fn from_parts(key: FontKey, mut skia_font: Font, swash_font: SwashFont) -> FontPair {
        skia_font.set_subpixel(true);
        skia_font.set_baseline_snap(true);
        skia_font.set_hinting(font_hinting(&key.hinting));
        skia_font.set_edging(font_edging(&key.edging));

        Self { key, skia_font, swash_font }
    }

    /// Axes that swash has to shape with, so that advances match the rasterized glyphs.
    pub fn variations(&self) -> &[FontVariation] {
        self.key.font_desc.as_ref().map(|desc| desc.variations.as_slice()).unwrap_or_default()
    }
}

fn font_data_and_index(typeface: &Typeface) -> Option<(Vec<u8>, usize)> {
    let (font_data, index) = typeface.to_font_bytes()?;
    // Only the lower 16 bits are part of the index, the rest indicates named instances. But we
    // don't care about those here, since we are just loading the font, so ignore them
    Some((font_data, (index & 0xFFFF) as usize))
}

/// Moves the typeface to the requested position in the variation design space.
fn apply_variations(
    typeface: Typeface,
    variations: &[FontVariation],
    collection_index: usize,
) -> Typeface {
    if variations.is_empty() {
        return typeface;
    }

    let axes = typeface.variation_design_parameters().unwrap_or_default();
    // Skia resets every axis that the position leaves out to the font default, so start from the
    // position that style matching resolved instead of from the requested axes alone.
    let mut position = typeface.variation_design_position().unwrap_or_default();

    for variation in variations {
        let tag = variation_tag(&variation.tag);

        let Some(axis) = axes.iter().find(|axis| axis.tag == tag) else {
            warn!("Font {} has no variation axis {}", typeface.family_name(), variation.tag);
            continue;
        };

        let value = variation.value.clamp(axis.min, axis.max);
        match position.iter_mut().find(|coordinate| coordinate.axis == tag) {
            Some(coordinate) => coordinate.value = value,
            None => position.push(Coordinate { axis: tag, value }),
        }
    }

    let mut arguments = FontArguments::new();
    // DirectWrite declines the clone when the index does not match the matched face.
    arguments.set_collection_index(collection_index);
    let arguments =
        arguments.set_variation_design_position(VariationPosition { coordinates: &position });

    match typeface.clone_with_arguments(&arguments) {
        Some(varied) => varied,
        None => {
            warn!("Font {} does not support variations", typeface.family_name());
            typeface
        }
    }
}

fn variation_tag(tag: &str) -> FourByteTag {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(tag.as_bytes());
    FourByteTag::from(u32::from_be_bytes(bytes))
}

impl PartialEq for FontPair {
    fn eq(&self, other: &Self) -> bool {
        self.swash_font.key == other.swash_font.key
    }
}

#[derive(Debug, Default, Hash, PartialEq, Eq, Clone)]
pub struct FontKey {
    // TODO(smolck): Could make these private and add constructor method(s)?
    // Would theoretically make things safer I guess, but not sure . . .
    pub font_desc: Option<FontDescription>,
    pub hinting: FontHinting,
    pub edging: FontEdging,
}

pub struct FontLoader {
    font_mgr: FontMgr,
    cache: LruCache<FontKey, Rc<FontPair>>,
    failed_fonts: HashSet<FontKey>,
    font_size: f32,
    last_resort: Option<Rc<FontPair>>,
}

impl Display for FontKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FontKey {{ font_desc: {:?}, hinting: {:?}, edging: {:?} }}",
            self.font_desc, self.hinting, self.edging
        )
    }
}

impl FontLoader {
    pub fn new(font_size: f32) -> FontLoader {
        FontLoader {
            font_mgr: FontMgr::new(),
            cache: LruCache::new(NonZeroUsize::new(20).unwrap()),
            failed_fonts: HashSet::new(),
            font_size,
            last_resort: None,
        }
    }

    fn load(&mut self, font_key: FontKey) -> Option<FontPair> {
        tracy_zone!("load_font");
        trace!("Loading font {font_key:?}");
        if let Some(desc) = &font_key.font_desc {
            let (family, style) = desc.as_family_and_font_style();
            let typeface = self.font_mgr.match_family_style(family, style)?;

            // Read the font data before cloning, a varied typeface has no bytes of its own.
            let (font_data, index) = font_data_and_index(&typeface)?;
            let swash_font = SwashFont::from_data(font_data, index)?;
            let typeface = apply_variations(typeface, &desc.variations, index);

            Some(FontPair::from_parts(
                font_key,
                Font::from_typeface(typeface, self.font_size),
                swash_font,
            ))
        } else {
            let data = Data::new_copy(DEFAULT_FONT);
            let typeface = self.font_mgr.new_from_data(data, 0)?;
            FontPair::new(font_key, Font::from_typeface(typeface, self.font_size))
        }
    }

    pub fn get_or_load(&mut self, font_key: &FontKey) -> Option<Rc<FontPair>> {
        if let Some(cached) = self.cache.get(font_key) {
            return Some(cached.clone());
        }

        if self.failed_fonts.contains(font_key) {
            return None;
        }

        let font = match self.load(font_key.clone()) {
            Some(loaded_font) => loaded_font,
            None => {
                self.failed_fonts.insert(font_key.clone());
                return None;
            }
        };

        let font = Rc::new(font);
        self.cache.put(font_key.clone(), font.clone());
        self.failed_fonts.remove(font_key);

        Some(font)
    }

    pub fn load_font_for_character(
        &mut self,
        coarse_style: CoarseStyle,
        character: char,
    ) -> Option<Rc<FontPair>> {
        let font_style = coarse_style.into();
        let typeface =
            self.font_mgr.match_family_style_character("", font_style, &[], character as i32)?;

        let font_key = FontKey {
            font_desc: Some(FontDescription {
                family: typeface.family_name(),
                style: coarse_style.name().map(str::to_string),
                ..Default::default()
            }),
            hinting: FontHinting::default(),
            edging: FontEdging::default(),
        };

        let font_pair = Rc::new(FontPair::new(
            font_key.clone(),
            Font::from_typeface(typeface, self.font_size),
        )?);

        self.cache.put(font_key, font_pair.clone());

        Some(font_pair)
    }

    pub fn get_or_load_last_resort(&mut self) -> Option<Rc<FontPair>> {
        if self.last_resort.is_some() {
            self.last_resort.clone()
        } else {
            let font_key = FontKey::default();
            let data = Data::new_copy(LAST_RESORT_FONT);

            let typeface = self.font_mgr.new_from_data(data, 0)?;
            let font_pair =
                Rc::new(FontPair::new(font_key, Font::from_typeface(typeface, self.font_size))?);

            self.last_resort = Some(font_pair.clone());
            Some(font_pair)
        }
    }

    pub fn loaded_fonts(&self) -> Vec<Rc<FontPair>> {
        self.cache.iter().map(|(_, v)| v.clone()).collect()
    }

    pub fn refresh(&mut self, font_pair: &FontPair) {
        self.cache.get(&font_pair.key);
    }

    pub fn font_names(&self) -> Vec<String> {
        self.font_mgr.family_names().collect()
    }
}

fn font_hinting(hinting: &FontHinting) -> SkiaHinting {
    match hinting {
        FontHinting::Full => SkiaHinting::Full,
        FontHinting::Slight => SkiaHinting::Slight,
        FontHinting::Normal => SkiaHinting::Normal,
        FontHinting::None => SkiaHinting::None,
    }
}

fn font_edging(edging: &FontEdging) -> SkiaEdging {
    match edging {
        FontEdging::AntiAlias => SkiaEdging::AntiAlias,
        FontEdging::Alias => SkiaEdging::Alias,
        FontEdging::SubpixelAntiAlias => SkiaEdging::SubpixelAntiAlias,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_failed_font_keys() {
        let mut loader = FontLoader::new(14.0);
        let missing_font = FontKey {
            font_desc: Some(FontDescription {
                family: "missing-font-family".to_string(),
                ..Default::default()
            }),
            hinting: FontHinting::default(),
            edging: FontEdging::default(),
        };

        assert!(loader.get_or_load(&missing_font).is_none());
        assert!(loader.failed_fonts.contains(&missing_font));

        assert!(loader.get_or_load(&missing_font).is_none());
        assert_eq!(loader.failed_fonts.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn renders_colrv1_glyphs() {
        use skia_safe::{Color, Paint, Point, surfaces};

        let data = Data::new_copy(font_test_data::COLRV0V1);
        let typeface = FontMgr::new().new_from_data(data, 0).unwrap();
        let font = Font::from_typeface(typeface, 96.0);
        let mut surface = surfaces::raster_n32_premul((128, 128)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(Color::TRANSPARENT);
        canvas.draw_str("\u{f0100}", Point::new(8.0, 96.0), &font, &Paint::default());

        let pixmap = surface.peek_pixels().unwrap();
        let has_colored_pixel = (0..pixmap.height()).any(|y| {
            (0..pixmap.width()).any(|x| {
                let color = pixmap.get_color((x, y));
                color.a() > 0 && (color.r() > 0 || color.g() > 0 || color.b() > 0)
            })
        });

        assert!(has_colored_pixel, "Skia did not render the COLRv1 glyph in color");
    }

    fn variable_typeface() -> Typeface {
        let data = Data::new_copy(font_test_data::VAZIRMATN_VAR);
        FontMgr::new().new_from_data(data, 0).unwrap()
    }

    fn weight_position(typeface: &Typeface) -> f32 {
        let position = typeface.variation_design_position().unwrap();
        let weight = variation_tag("wght");

        position.iter().find(|coordinate| coordinate.axis == weight).unwrap().value
    }

    #[test]
    fn moves_typeface_to_requested_axis_value() {
        let variations = vec![FontVariation { tag: "wght".to_string(), value: 700.0 }];

        let varied = apply_variations(variable_typeface(), &variations, 0);

        assert_eq!(weight_position(&varied), 700.0);
    }

    #[test]
    fn clamps_axis_value_to_the_font_range() {
        let variations = vec![FontVariation { tag: "wght".to_string(), value: 5000.0 }];

        let varied = apply_variations(variable_typeface(), &variations, 0);
        let max = varied
            .variation_design_parameters()
            .unwrap()
            .iter()
            .find(|axis| axis.tag == variation_tag("wght"))
            .unwrap()
            .max;

        assert_eq!(weight_position(&varied), max);
    }

    #[test]
    fn ignores_axes_the_font_does_not_have() {
        let typeface = variable_typeface();
        let default_weight = weight_position(&typeface);
        let variations = vec![FontVariation { tag: "CASL".to_string(), value: 1.0 }];

        let varied = apply_variations(typeface, &variations, 0);

        assert_eq!(weight_position(&varied), default_weight);
    }
}
