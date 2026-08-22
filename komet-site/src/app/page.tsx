import Nav from "@/components/Nav";
import Hero from "@/components/Hero";
import AgentMarquee from "@/components/AgentMarquee";
import Features from "@/components/Features";
import Stats from "@/components/Stats";
import Downloads from "@/components/Downloads";
import Faq from "@/components/Faq";
import Footer from "@/components/Footer";

export default function Home() {
  return (
    <>
      <Nav />
      <main>
        <Hero />
        <AgentMarquee />
        <Features />
        <Stats />
        <Downloads />
        <Faq />
      </main>
      <Footer />
    </>
  );
}
