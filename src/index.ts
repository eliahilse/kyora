import { Bot } from "grammy";

const bot = new Bot(process.env.BOT_TOKEN!);

async function sendMessage() {
    try {
        await bot.api.sendMessage(
            process.env.CHANNEL_ID!,
            "Hello World",
            {
                message_thread_id: Number(process.env.TOPIC_ID)
            }
        );
        console.log("Message sent successfully!");
    } catch (error) {
        console.error("Failed to send message:", error);
    }
}

sendMessage();