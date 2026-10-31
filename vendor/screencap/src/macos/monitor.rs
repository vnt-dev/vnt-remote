use core_graphics::display::CGDisplay;
use std::io;

#[derive(Clone, Debug)]
pub struct Monitor {
    index: u32,
    display: CGDisplay,
}
impl Monitor {
    pub fn primary() -> io::Result<Self> {
        Self::from_index(0)
    }
    pub fn from_index(index: u32) -> io::Result<Self> {
        let monitors = Self::all()?;
        monitors
            .get(index as usize)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Monitor index out of range"))
    }
    pub fn all() -> io::Result<Vec<Self>> {
        let list = CGDisplay::active_displays().map_err(|e| io::Error::other(format!("{e}")))?;
        let mut rs = Vec::with_capacity(list.len());
        if list.is_empty() {
            return Ok(rs);
        }
        let main = CGDisplay::main();

        rs.push(Self {
            index: 0,
            display: main,
        });
        for id in list {
            if id == main.id {
                continue;
            }
            let index = rs.len() as u32;
            rs.push(Self {
                index,
                display: CGDisplay::new(id),
            });
        }
        Ok(rs)
    }
    pub fn index(&self) -> u32 {
        self.index
    }
    pub fn display(&self) -> &CGDisplay {
        &self.display
    }
    pub fn size(&self) -> io::Result<(u32, u32)> {
        if let Some(model) = self.display.display_mode() {
            let width = model.pixel_width() as u32;
            let height = model.pixel_height() as u32;
            Ok((width, height))
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "No display modes"))
        }
    }
    pub fn scale(&self) -> io::Result<(f64, f64)> {
        if let Some(model) = self.display.display_mode() {
            Ok((
                model.pixel_width() as f64 / model.width() as f64,
                model.pixel_height() as f64 / model.height() as f64,
            ))
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "No display modes"))
        }
    }
    pub fn scale_size(&self) -> io::Result<(u32, f64, u32, f64)> {
        if let Some(model) = self.display.display_mode() {
            Ok((
                model.pixel_width() as u32,
                model.pixel_width() as f64 / model.width() as f64,
                model.pixel_height() as u32,
                model.pixel_height() as f64 / model.height() as f64,
            ))
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "No display modes"))
        }
    }
}
