use {
    super::{DEFAULT_FONT_SIZE, DEFAULT_HEADER_FONT_SIZE, DEFAULT_PADDING},
    crate::gui::{style, Interaction, Message},
    crate::localization::localized_string,
    ajour_core::{theme::ColorPalette},
    iced::{
        button, scrollable, Button, Column, Container, Element, Length, Row, Scrollable, Space,
        Text,
    },
    std::collections::HashMap,
    strfmt::strfmt,
};

pub fn data_container<'a>(
    color_palette: ColorPalette,
    scrollable_state: &'a mut scrollable::State,
    website_button_state: &'a mut button::State,
    donation_button_state: &'a mut button::State,
) -> Container<'a, Message> {
    let ajour_title = Text::new(localized_string("ajour")).size(DEFAULT_HEADER_FONT_SIZE);
    let ajour_title_container =
        Container::new(ajour_title).style(style::BrightBackgroundContainer(color_palette));

    let website_button: Element<Interaction> = Button::new(
        website_button_state,
        Text::new(localized_string("website")).size(DEFAULT_FONT_SIZE),
    )
    .style(style::DefaultBoxedButton(color_palette))
    .on_press(Interaction::OpenLink(localized_string("website-http")))
    .into();

    let donation_button: Element<Interaction> = Button::new(
        donation_button_state,
        Text::new(localized_string("donate")).size(DEFAULT_FONT_SIZE),
    )
    .style(style::DefaultBoxedButton(color_palette))
    .on_press(Interaction::OpenLink(localized_string("donate-http")))
    .into();

    let button_row = Row::new()
        .spacing(DEFAULT_PADDING)
        .push(website_button.map(Message::Interaction))
        .push(donation_button.map(Message::Interaction));

    let mut scrollable = Scrollable::new(scrollable_state)
        .spacing(1)
        .height(Length::FillPortion(1))
        .style(style::Scrollable(color_palette));

    scrollable = scrollable
        .push(ajour_title_container)
        .push(Space::new(Length::Units(0), Length::Units(DEFAULT_PADDING)))
        .push(button_row)
        .push(Space::new(Length::Units(0), Length::Units(DEFAULT_PADDING)));

    let col = Column::new().push(scrollable);
    let row = Row::new()
        .push(Space::new(Length::Units(DEFAULT_PADDING), Length::Units(0)))
        .push(col);

    // Returns the final container.
    Container::new(row)
        .center_x()
        .width(Length::Fill)
        .height(Length::Shrink)
        .style(style::NormalBackgroundContainer(color_palette))
        .padding(20)
}
